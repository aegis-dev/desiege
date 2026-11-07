//! tank list <file.dsres>            all paths with sizes
//! tank cat  <file.dsres> <path>     one file to stdout
use std::io::Write;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, file] if cmd == "list" => {
            let t = tank::Tank::open(file)?;
            let mut out = std::io::stdout().lock();
            for p in t.paths() {
                writeln!(out, "{:>10} {p}", t.size(p).unwrap())?;
            }
        }
        [cmd, file, path] if cmd == "cat" => std::io::stdout().write_all(&tank::Tank::open(file)?.read(path)?)?,
        _ => eprintln!("usage: tank list <file> | tank cat <file> <path>"),
    }
    Ok(())
}
