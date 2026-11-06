#!/usr/bin/env python3
"""Release what a commit on main versions. The release jobs of .github/workflows/ci.yml run it once every check passed.

A release unit is the packages that share the workspace version (`version.workspace = true`), or one package with a
version of its own. A unit is released when its version moves: one of its publishable crates has a version crates.io
does not have yet, or it ships binaries (RELEASE_BINS), has no GitHub Release of that version yet, and the version
differs from the parent commit's or is newer than the unit's latest tag. Releasing a unit
tags the commit (`v<version>` for the workspace unit or a repository's only unit, `<package>-v<version>` otherwise),
publishes its crates to crates.io in dependency order and creates the GitHub Release: notes from the version's
CHANGELOG section (else the commits since the unit's previous tag), the binaries and their SHA256SUMS attached.
Every step skips what is already done, so a rerun of a failed release finishes it.

  release.py plan            print the plan; in a GitHub job also as step outputs (plan, crates, bins)
  release.py verify          cargo publish --dry-run of the planned crates together (no token needed)
  release.py build TARGET    build the planned binaries for TARGET into dist/
  release.py publish [--dry-run]   tag, publish and create the Releases (with --dry-run: say what it would do)

Run it from the cargo workspace's folder. Environment: RELEASE_BINS (the downloads: see compute_plan),
RELEASE_DIST (the archives; dist/), PLAN (the plan job's output; computed again when unset), GH_TOKEN (gh),
CARGO_REGISTRY_TOKEN (cargo publish).
"""

import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import zipfile

ROOT = pathlib.Path.cwd()  # the cargo workspace
DIST = pathlib.Path(os.environ.get("RELEASE_DIST") or ROOT / "dist")  # the binary archives
LOCKED = ["--locked"] if (ROOT / "Cargo.lock").exists() else []  # a library repository may not commit its lock file


def run(*cmd, check=True, capture=True):
    r = subprocess.run(cmd, cwd=ROOT, capture_output=capture, text=True)
    if check and r.returncode:
        sys.exit(f"{' '.join(cmd)} failed ({r.returncode}):\n{r.stdout or ''}{r.stderr or ''}")
    return r


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def index_versions(name):
    """The versions crates.io has of `name` (yanked ones too: they can't be published again), None if it has none."""
    n = name.lower()
    path = {1: f"1/{n}", 2: f"2/{n}", 3: f"3/{n[0]}/{n}"}.get(len(n), f"{n[:2]}/{n[2:4]}/{n}")
    req = urllib.request.Request(f"https://index.crates.io/{path}", headers={"Cache-Control": "no-cache"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return {json.loads(line)["vers"] for line in r.read().decode().splitlines() if line.strip()}
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def inherits_version(manifest):
    v = tomllib.loads(pathlib.Path(manifest).read_text(encoding="utf-8"))["package"].get("version")
    return isinstance(v, dict) and v.get("workspace") is True


def tag_commit(tag):
    r = run("git", "rev-parse", "-q", "--verify", f"refs/tags/{tag}^{{commit}}", check=False)
    return r.stdout.strip() or None


def release_tags():
    out = run("gh", "release", "list", "--limit", "1000", "--json", "tagName").stdout
    return {r["tagName"] for r in json.loads(out)}


def parse_version(v):
    return tuple(int(x) for x in re.match(r"(\d+)\.(\d+)\.(\d+)", v).groups())


def version_moved(u, pkgs):
    """Whether a unit's version moved: it differs from the parent commit's, or is newer than the unit's latest tag. A
    run started by hand (a private repository's release) is the request itself: its version moved if unreleased."""
    if os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch":
        return True
    prefix = u["tag"][: -len(u["version"])]
    tagged = [t[len(prefix):] for t in run("git", "tag", "--list", f"{prefix}*").stdout.split()]
    tagged = [v for v in tagged if re.fullmatch(r"\d+\.\d+\.\d+.*", v)]
    if tagged and parse_version(u["version"]) > max(map(parse_version, tagged)):
        return True
    top = pathlib.Path(run("git", "rev-parse", "--show-toplevel").stdout.strip())
    manifest = pathlib.Path(pkgs[u["package"]]["manifest_path"]) if u["package"] else ROOT / "Cargo.toml"
    old = run("git", "show", f"HEAD^:{manifest.resolve().relative_to(top.resolve()).as_posix()}", check=False)
    if old.returncode:
        if re.search(r"does not exist|exists on disk, but not in|invalid object name", old.stderr):
            return True  # a new package (or the first commit)
        sys.exit(f"git show HEAD^:{manifest}: {old.stderr}")
    t = tomllib.loads(old.stdout)
    v = t.get("package", {}).get("version") if u["package"] else t.get("workspace", {}).get("package", {}).get("version")
    return v != u["version"]


def downloads(spec, pkgs):
    """RELEASE_BINS's downloads: each word is `name`, one binary or library (cdylib) target shipped alone, or
    `name=a+b+c`, an archive `name` of the targets a, b and c (all of one release unit); `:windows` / `:linux` after
    it ships it only for targets whose triple names that. Each is {"bin": archive name, "only", "parts": [{"package",
    "target", "kind": "bin" or "cdylib"}]}."""
    shippable = {}
    for p in pkgs.values():
        for t in p["targets"]:
            for kind in ("bin", "cdylib"):
                if kind in t["kind"]:
                    shippable.setdefault(t["name"], []).append({"package": p["name"], "target": t["name"],
                                                                "kind": kind})
    out, missing = [], []
    for word in spec.split():
        name, _, only = word.partition(":")
        archive, _, listed = name.partition("=")
        parts = []
        for target in listed.split("+") if listed else [archive]:
            found = shippable.get(target, [])
            if len(found) > 1:
                sys.exit(f"RELEASE_BINS: {target} is a target of {' and '.join(f['package'] for f in found)}")
            if found:
                parts += found
            else:
                missing.append(target)
        out.append({"bin": archive, "only": only, "parts": parts})
    if missing:
        sys.exit(f"RELEASE_BINS names binaries or libraries no workspace package has: {' '.join(sorted(missing))}")
    return out


def compute_plan():
    meta = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1").stdout)
    members = set(meta["workspace_members"])
    pkgs = {p["name"]: p for p in meta["packages"] if p["id"] in members}
    unit_of = {n: "" if inherits_version(p["manifest_path"]) else n for n, p in pkgs.items()}
    shipped = downloads(os.environ.get("RELEASE_BINS", ""), pkgs)
    head = run("git", "rev-parse", "HEAD").stdout.strip()

    units = {}
    for p in pkgs.values():
        publishable = p["publish"] is None or "crates-io" in p["publish"]
        key = unit_of[p["name"]]
        if publishable or any(unit_of[part["package"]] == key for d in shipped for part in d["parts"]):
            u = units.setdefault(key, {"package": key, "version": p["version"], "crates": [], "bins": []})
            if publishable:
                u["crates"].append(p["name"])
    for d in shipped:
        keys = {unit_of[part["package"]] for part in d["parts"]}
        if len(keys) != 1:
            sys.exit(f"RELEASE_BINS: {d['bin']} holds targets of packages released apart: "
                     f"{' '.join(sorted(part['package'] for part in d['parts']))}")
        units[keys.pop()]["bins"].append(d)

    # the dependencies crates.io needs first: normal and build ones, and dev ones that carry a version (path-only
    # dev-dependencies are dropped on publish)
    deps = {
        n: {d["name"] for d in p["dependencies"] if d["name"] in pkgs and (d["kind"] != "dev" or d["req"] != "*")}
        for n, p in pkgs.items()
    }
    releases = set(release_tags()) if units else set()
    plan = []
    for key, u in sorted(units.items()):
        u["tag"] = f"v{u['version']}" if key == "" or len(units) == 1 else f"{key}-v{u['version']}"
        u["title"] = f"{key or os.environ.get('GITHUB_REPOSITORY', ROOT.name).split('/')[-1]} {u['version']}"
        pending, never = [], []
        for c in u["crates"]:
            have = index_versions(c)
            if have is None:
                never.append(c)
            elif pkgs[c]["version"] not in have:
                pending.append(c)
        tagged = tag_commit(u["tag"])
        released = u["tag"] in releases
        if pending and never:
            sys.exit(f"{u['title']}: {' '.join(never)} has never been published, and trusted publishing can't make "
                     "a crate's first release: the token holder publishes it first (CONTRIBUTING.md, Releasing)")
        if never:
            print(f"::warning::{' '.join(never)} not on crates.io yet: its first publish needs the token holder")
        if pending or (u["bins"] and not released and version_moved(u, pkgs)) or (tagged == head and not released):
            u["publish"] = pending
            u["crates"] = [c for c in u["crates"] if c not in never]
            plan.append(u)
    # crates in dependency order (across units too)
    order, seen = [], set()

    def visit(n, path=()):
        if n in path:
            sys.exit(f"dependency cycle: {' -> '.join(path + (n,))}")
        if n not in seen:
            for d in sorted(deps[n]):
                visit(d, path + (n,))
            seen.add(n)
            order.append(n)

    for n in sorted(pkgs):
        visit(n)
    for u in plan:
        u["publish"].sort(key=order.index)
    plan.sort(key=lambda u: min((order.index(c) for c in u["publish"]), default=len(order)))
    return plan


def load_plan():
    return json.loads(os.environ["PLAN"]) if os.environ.get("PLAN") else compute_plan()


def plan_cmd():
    plan = compute_plan()
    for u in plan:
        log(f"release {u['title']} as {u['tag']}: publish {' '.join(u['publish']) or 'nothing'}; "
            f"downloads {' '.join(b['bin'] for b in u['bins']) or 'none'}")
    if not plan:
        log("nothing to release: no version moved")
    outputs = {
        "plan": json.dumps(plan, separators=(",", ":")),
        "crates": "true" if any(u["publish"] for u in plan) else "",
        "bins": "true" if any(u["bins"] for u in plan) else "",
    }
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as f:
            f.writelines(f"{k}={v}\n" for k, v in outputs.items())
    print(outputs["plan"])


def verify_cmd():
    crates = [c for u in load_plan() for c in u["publish"]]
    if crates:
        # together, so a crate is verified against the unpublished versions of its siblings
        run("cargo", "publish", "--dry-run", *LOCKED, *[a for c in crates for a in ("-p", c)], capture=False)


def parts_of(b):
    """A download's targets (plans from before downloads had parts name one binary)."""
    return b.get("parts") or [{"package": b["package"], "target": b["bin"], "kind": "bin"}]


def artifact(target, part):
    """The file cargo builds for a part on `target`."""
    windows = "windows" in target
    if part["kind"] == "bin":
        return part["target"] + (".exe" if windows else "")
    name = part["target"].replace("-", "_")
    return f"{name}.dll" if windows else f"lib{name}.dylib" if "apple" in target else f"lib{name}.so"


def build_cmd(target):
    DIST.mkdir(exist_ok=True)
    for u in load_plan():
        for b in u["bins"]:
            if b.get("only") and b["only"] not in target:
                continue
            parts = parts_of(b)
            # One build of every part, so each is built with the same features as the others it ships beside
            libs = sorted({p["package"] for p in parts if p["kind"] == "cdylib"})
            bins = [p for p in parts if p["kind"] == "bin"]
            if libs:
                run("cargo", "build", "--release", *LOCKED, "--target", target, "--lib",
                    *[a for p in libs for a in ("-p", p)], capture=False)
            if bins:
                run("cargo", "build", "--release", *LOCKED, "--target", target,
                    *[a for p in bins for a in ("-p", p["package"], "--bin", p["target"])], capture=False)
            built = [ROOT / "target" / target / "release" / artifact(target, p) for p in parts]
            extra = [p for p in sorted(ROOT.iterdir()) if re.match(r"(LICEN[CS]E|COPYING)", p.name, re.I)]
            stem = f"{b['bin']}-{u['version']}-{target}"
            if "windows" in target:
                with zipfile.ZipFile(DIST / f"{stem}.zip", "w", zipfile.ZIP_DEFLATED) as z:
                    for p in [*built, *extra]:
                        z.write(p, f"{stem}/{p.name}")
            else:
                with tarfile.open(DIST / f"{stem}.tar.gz", "w:gz") as t:
                    for p in [*built, *extra]:
                        t.add(p, f"{stem}/{p.name}")
            log(f"built {stem}: {' '.join(p.name for p in built)}")


def notes(u):
    """The version's CHANGELOG section, else the commits since the unit's previous tag."""
    dirs = [ROOT]
    if u["package"]:
        meta = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1").stdout)
        dirs.insert(0, next(pathlib.Path(p["manifest_path"]).parent for p in meta["packages"]
                            if p["name"] == u["package"]))
    for d in dirs:
        f = d / "CHANGELOG.md"
        if not f.exists():
            continue
        lines = f.read_text(encoding="utf-8").splitlines()
        head = re.compile(rf"^(#+)\s*\[?v?{re.escape(u['version'])}\]?(?=[\s(:-]|$)")
        for i, line in enumerate(lines):
            m = head.match(line)
            if m:
                body = []
                for rest in lines[i + 1:]:
                    h = re.match(r"^(#+)\s", rest)
                    if h and len(h.group(1)) <= len(m.group(1)):
                        break
                    body.append(rest)
                text = "\n".join(body).strip()
                if text:
                    return text
    prefix = u["tag"][: -len(u["version"])]
    tags = run("git", "tag", "--list", f"{prefix}*", "--merged", "HEAD", "--sort=-v:refname").stdout.split()
    prev = next((t for t in tags if t != u["tag"]), None)
    where = ["--", os.path.relpath(dirs[0], ROOT)] if u["package"] else []
    commits = run("git", "log", "--format=- %s (%h)", f"{prev}..HEAD" if prev else "HEAD", *where).stdout.splitlines()
    more = f"\n\n…and {len(commits) - 100} earlier commits" if len(commits) > 100 else ""
    return "\n".join(commits[:100]) + more


def publish_crate(name, dry):
    if dry:
        log(f"would publish {name}")
        return
    for attempt in range(8):
        r = run("cargo", "publish", *LOCKED, "--no-verify", "-p", name, check=False)
        if r.returncode == 0:
            log(f"published {name}")
            return
        err = r.stderr
        if "already exists" in err:
            log(f"{name} is already published at this version")
            return
        if "429" in err or "Too Many Requests" in err:
            wait = 600
        elif "failed to select a version" in err or "no matching package" in err:
            wait = 30  # a sibling just published and the index has not caught up
        else:
            sys.exit(f"cargo publish -p {name} failed:\n{err}")
        log(f"publishing {name} must wait ({err.strip().splitlines()[-1]}); retrying in {wait}s")
        time.sleep(wait)
    sys.exit(f"cargo publish -p {name}: gave up after 8 attempts")


def publish_cmd(dry):
    head = run("git", "rev-parse", "HEAD").stdout.strip()
    plan = load_plan()
    for u in plan:
        # the tag first, so a rerun after a failed publish still finds the commit the release belongs to
        tagged = tag_commit(u["tag"])
        if tagged != head:
            # a tag elsewhere means an earlier attempt at this version that never released: it moves here
            if dry:
                log(f"would tag {head[:12]} {u['tag']}" + (f" (moving it from {tagged[:12]})" if tagged else ""))
            else:
                run("git", "tag", "-f", u["tag"], head)
                run("git", "push", "-f", "origin", f"refs/tags/{u['tag']}")
        for c in u["publish"]:
            publish_crate(c, dry)
    releases = release_tags()
    for u in plan:
        if u["tag"] in releases:
            continue
        crates = "".join(f"\n- [{c} {u['version']}](https://crates.io/crates/{c}/{u['version']})" for c in u["crates"])
        body = notes(u) + (f"\n\n## Crates\n{crates}" if crates else "")
        assets = sorted(p for p in DIST.glob("*") if any(p.name.startswith(f"{b['bin']}-{u['version']}-")
                                                            for b in u["bins"])) if DIST.exists() else []
        expected = len(u["bins"])
        if u["bins"] and len(assets) < expected:
            sys.exit(f"{u['title']}: {len(assets)} binary archives in dist/, expected at least {expected}")
        work = pathlib.Path(tempfile.mkdtemp())
        (work / "notes.md").write_text(body + "\n", encoding="utf-8")
        files = [str(p) for p in assets]
        if assets:
            sums = work / "SHA256SUMS"
            sums.write_text("".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in assets))
            files.append(str(sums))
        if dry:
            log(f"would create Release {u['tag']} \"{u['title']}\" with {[pathlib.Path(f).name for f in files]}:\n{body}")
        else:
            run("gh", "release", "create", u["tag"], "--verify-tag", "--title", u["title"], "--notes-file",
                str(work / "notes.md"), *files)
            log(f"created Release {u['tag']}")
        shutil.rmtree(work)


if __name__ == "__main__":
    cmd = sys.argv[1:2]
    if cmd == ["plan"]:
        plan_cmd()
    elif cmd == ["verify"]:
        verify_cmd()
    elif cmd == ["build"] and len(sys.argv) == 3:
        build_cmd(sys.argv[2])
    elif cmd == ["publish"]:
        publish_cmd("--dry-run" in sys.argv[2:])
    else:
        sys.exit(__doc__)
