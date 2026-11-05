//! Game values: the `table.txt` format that names a game's values, functions and actions for one executable
//! ([`table`]), the expressions in it and in `do` requests ([`expr`]), and finding the table's symbols in the
//! executable's code ([`image`]), the same for the running game (locum) as for the file on disk
//! (`locum-cli table`).

pub mod expr;
pub mod image;
pub mod table;
