//! `fs.squashfs <target> <verb>`: an errand inside a SquashFS image,
//! without mounting it.
//!
//! The verbs are the shared set every `fs.<fs>` answers: `ls`, `stat`, `read`,
//! `get`/`info`, and `write`, `mkdir`, `rm`, `rmdir`, `mv`, `ln`, `truncate`,
//! `set`, `resize`. SquashFS is a read-only format, so mutations refuse with exit status 3
//! and "SquashFS is read-only": a script moved here from a writable
//! filesystem fails loudly instead of meaning something else.
//!
//! Metadata is JSON (or `--text`); file content is raw bytes. This replaces
//! `lssquashfs`: its `info` is `get`, `cat` is `read`, `readlink` is the
//! `target` of a symlink's `ls` entry, and `ls --recursive` walks a tree
//! within explicit depth and entry bounds.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_core::{BlockRead, FileDevice, OwnedSlice};
use fs_squashfs::{FileType, Filesystem, Inode};

pub const TOOL: Tool = Tool {
    name: "fs.squashfs",
    verb: "fs",
    section: 1,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "List, read and inspect a SquashFS image without mounting it",
    command,
    run,
};

/// The canonical keys every `fs.<fs>` answers, in the shared order.
/// SquashFS's own are nested under `squashfs`.
pub const KEYS: &[&str] = &[
    "fs",
    "label",
    "total_bytes",
    "free_bytes",
    "block_size",
    "dirty",
    "squashfs",
];

/// What every verb that would change the image answers.
const READ_ONLY: &str = "SquashFS is read-only";

/// The superblock's flag bits, by the names squashfs-tools gives them.
const FLAGS: &[(u16, &str)] = &[
    (0x0001, "uncompressed_inodes"),
    (0x0002, "uncompressed_data"),
    (0x0004, "check"),
    (0x0008, "uncompressed_fragments"),
    (0x0010, "no_fragments"),
    (0x0020, "always_fragments"),
    (0x0040, "duplicates"),
    (0x0080, "exportable"),
    (0x0100, "uncompressed_xattrs"),
    (0x0200, "no_xattrs"),
    (0x0400, "compressor_options"),
    (0x0800, "uncompressed_ids"),
];

fn command() -> Cmd {
    Cmd::new("fs.squashfs")
        .about("List, read and inspect a SquashFS image without mounting it")
        .long_about(
            "Work inside a SquashFS image directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, see what is in it, check how \
             it was built), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the file's raw \
             bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being the \
             exit status: 1 failed, 2 wrong command line, 3 SquashFS cannot do that (it is \
             read-only).",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("The image file or device")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help("Where the filesystem starts in TARGET, for one embedded in a larger image")
                .value_parser(value_parser!(u64))
                .global(true),
        )
        .args(fs_core::cli::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("recursive")
                        .long("recursive")
                        .short('R')
                        .action(ArgAction::SetTrue)
                        .help("Walk depth first, sorting names as bytes; never follow symlinks"),
                )
                .arg(
                    Arg::new("max-depth")
                        .long("max-depth")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help(
                            "Maximum recursive depth (default: 64); direct children have depth 1",
                        ),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help("Maximum recursive entries (default: 100000)"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.squashfs rootfs.sqfs ls /etc\n  \
                     fs.squashfs rootfs.sqfs ls / | jq -r '.[].name'\n  \
                     fs.squashfs rootfs.sqfs ls /bin/sh | jq -r '.[0].target'\n  \
                     fs.squashfs rootfs.sqfs ls --text /\n  \
                     fs.squashfs rootfs.sqfs ls -R --max-depth 8 --limit 1000 /etc\n\n\
                     Recursive entries add volume-absolute path and depth. The starting \
                     directory is omitted; a starting file or symlink has depth 0. A repeated \
                     directory inode is listed with revisited: true and not entered. If another \
                     entry would exceed either bound, or a directory cannot be read, the \
                     collected prefix stays on stdout and an error names the bound or path \
                     on stderr (exit 1). Exact fits succeed; zero allows no entries or depth.",
                ),
        )
        .subcommand(
            Cmd::new("stat")
                .about("Report one path: identity, type, mode, owner, links, sizes and stored time")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.squashfs rootfs.sqfs stat /etc/hostname\n  \
                     fs.squashfs rootfs.sqfs stat /var/log | jq .nlink\n  \
                     fs.squashfs rootfs.sqfs stat --text /bin/sh\n\n\
                     A symlink is reported, not followed. SquashFS stores only mtime, as \
                     whole seconds since the epoch. Other times, nanoseconds and allocated_bytes \
                     are null: the format stores no per-file allocation total. Raw inode type, \
                     fragment index/offset, block count and xattr index are under squashfs; \
                     inapplicable fields are null.",
                ),
        )
        .subcommand(
            Cmd::new("read")
                .about("Write a file's bytes to stdout, or to a file with -o")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.squashfs rootfs.sqfs read /etc/hostname\n  \
                     fs.squashfs rootfs.sqfs read /var/log/syslog | grep -i error\n  \
                     fs.squashfs rootfs.sqfs read /backup.tar -o backup.tar",
                ),
        )
        .subcommand(
            refused_command(
                "write",
                "fs.squashfs rootfs.sqfs write /etc/hostname < hostname\n  \
                 fs.squashfs rootfs.sqfs write /log --at 1024 < chunk",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("at")
                    .long("at")
                    .value_name("BYTES")
                    .value_parser(value_parser!(u64))
                    .help("Position inside the file, not the filesystem's --offset in TARGET"),
            ),
        )
        .subcommand(
            refused_command(
                "mkdir",
                "fs.squashfs rootfs.sqfs mkdir /backup\n  \
                 fs.squashfs rootfs.sqfs mkdir --parents /backup/2026",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("parents")
                    .long("parents")
                    .short('p')
                    .action(ArgAction::SetTrue)
                    .help("Create missing parent directories too"),
            ),
        )
        .subcommand(
            refused_command("rm", "fs.squashfs rootfs.sqfs rm /notes.txt").arg(mutation_path()),
        )
        .subcommand(
            refused_command("rmdir", "fs.squashfs rootfs.sqfs rmdir /backup").arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "mv",
                "fs.squashfs rootfs.sqfs mv /notes.txt /archive/notes.txt",
            )
            .arg(
                Arg::new("source")
                    .value_name("SOURCE")
                    .required(true)
                    .value_parser(value_parser!(OsString)),
            )
            .arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "ln",
                "fs.squashfs rootfs.sqfs ln /notes.txt /also-notes.txt\n  \
                 fs.squashfs rootfs.sqfs ln -s ../etc/hostname /hostname",
            )
            .arg(
                Arg::new("symbolic")
                    .short('s')
                    .long("symbolic")
                    .action(ArgAction::SetTrue)
                    .help("Make a symbolic link whose target is TARGET, as given"),
            )
            .arg(
                Arg::new("target")
                    .value_name("TARGET")
                    .required(true)
                    .value_parser(value_parser!(OsString)),
            )
            .arg(mutation_path()),
        )
        .subcommand(
            refused_command(
                "truncate",
                "fs.squashfs rootfs.sqfs truncate /log.txt 0\n  \
                 fs.squashfs rootfs.sqfs truncate /sparse.img 1073741824",
            )
            .arg(mutation_path())
            .arg(
                Arg::new("size")
                    .value_name("BYTES")
                    .required(true)
                    .value_parser(value_parser!(u64)),
            ),
        )
        .subcommand(key_command(
            "get",
            "Report the filesystem's properties, or one of them",
        ))
        .subcommand(key_command(
            "info",
            "The same as get: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("set")
                .about("Refused: SquashFS is read-only")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.squashfs rootfs.sqfs set label BACKUP\n\n\
                     Answers `SquashFS is read-only` (exit 3). The format has no label at \
                     all: `get label` is null.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Refused: SquashFS is read-only")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Do it without asking"),
                )
                .after_help(
                    "Examples:\n  fs.squashfs rootfs.sqfs resize 1G --force\n\n\
                     Answers `SquashFS is read-only` (exit 3).",
                ),
        )
        .after_help(
            "Examples:\n  fs.squashfs rootfs.sqfs ls /\n  \
             fs.squashfs rootfs.sqfs read /etc/fstab > fstab\n  \
             fs.squashfs rootfs.sqfs get squashfs.compression --text\n  \
             fs.squashfs --offset 1048576 firmware.bin info",
        )
}

fn refused_command(name: &'static str, examples: &'static str) -> Cmd {
    Cmd::new(name)
        .about("Refused: SquashFS is read-only")
        .after_help(format!(
            "Examples:\n  {examples}\n\n\
             Answers `SquashFS is read-only` (exit 3) before opening the image or reading \
             stdin. Rebuild the image with mksquashfs instead."
        ))
}

fn mutation_path() -> Arg {
    Arg::new("path")
        .value_name("PATH")
        .required(true)
        .value_parser(value_parser!(OsString))
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or squashfs.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  fs.squashfs rootfs.sqfs {name}\n  \
             fs.squashfs rootfs.sqfs {name} block_size --text\n  \
             fs.squashfs rootfs.sqfs {name} squashfs.compression"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    let offset = sub
        .get_one::<u64>("offset")
        .or_else(|| matches.get_one::<u64>("offset"))
        .copied()
        .unwrap_or(0);
    match verb {
        "ls" if sub.get_flag("recursive") => {
            ls_recursive(&open(target, offset)?, path_arg(sub), sub)
        }
        "ls" => ls(&open(target, offset)?, path_arg(sub)),
        "stat" => stat(&open(target, offset)?, path_arg(sub)),
        "read" => read(&open(target, offset)?, path_arg(sub), sub.get_one("output")),
        "get" | "info" => get(
            &open(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        // Refused before the image is opened, and `write` before stdin is
        // read: nothing about the target can change the answer.
        "write" | "mkdir" | "rm" | "rmdir" | "mv" | "ln" | "truncate" | "set" | "resize" => {
            Err(CliError::refused(format!(
                "{verb}: {READ_ONLY}; rebuild the image with mksquashfs instead"
            )))
        }
        other => unreachable!("clap knows no verb {other}"),
    }
}

fn path_arg(sub: &ArgMatches) -> &[u8] {
    let path = sub
        .get_one::<OsString>("path")
        .expect("clap requires or defaults the path");
    os_bytes(path)
}

#[cfg(unix)]
fn os_bytes(s: &OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsString) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Open `target` read-only, `offset` bytes in, as a SquashFS image.
fn open(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    let name = target.to_string_lossy();
    let file = FileDevice::open(std::path::Path::new(target))
        .map_err(|e| CliError::failed(format!("open {name}: {e}")))?;
    let size = file.size_bytes();
    let dev: Arc<dyn BlockRead> = if offset == 0 {
        Arc::new(file)
    } else if offset >= size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of {name} ({size} bytes)"
        )));
    } else {
        Arc::new(OwnedSlice::new(Arc::new(file), offset, size - offset))
    };
    Filesystem::open(dev)
        .map_err(|e| CliError::failed(format!("{name} is not a readable SquashFS image: {e}")))
}

fn squashfs_error(what: &[u8], e: fs_squashfs::Error) -> CliError {
    CliError::failed(format!("{}: {e}", show(what)))
}

fn type_name(t: FileType) -> &'static str {
    match t {
        FileType::RegFile => "file",
        FileType::Dir => "dir",
        FileType::Symlink => "symlink",
        FileType::CharDev => "char",
        FileType::BlockDev => "block",
        FileType::Fifo => "fifo",
        FileType::Socket => "socket",
        FileType::Unknown => "unknown",
    }
}

fn type_char(t: &str) -> char {
    match t {
        "file" => '-',
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '?',
    }
}

/// One `ls` entry: the fields every `fs.<fs>` reports, typed the same way
/// everywhere -- name (string), type (string), size (number), mode (octal
/// string), mtime (seconds since the epoch, number), inode (number), and
/// target (string) for a symlink. A name that is not UTF-8 is shown
/// lossily, with its exact bytes in `name_hex`. A device carries its
/// `major` and `minor` too, which is what `ls -l` shows in place of a size.
fn entry(name: &[u8], inode: &Inode) -> Json {
    let kind = inode.file_type();
    let mut fields = vec![("name", Json::from(show(name)))];
    if std::str::from_utf8(name).is_err() {
        fields.push((
            "name_hex",
            Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("type", Json::from(type_name(kind))),
        ("size", Json::from(inode.file_size)),
        (
            "mode",
            Json::from(format!("{:04o}", inode.permissions & 0o7777)),
        ),
        ("mtime", Json::from(inode.mtime)),
        ("inode", Json::from(inode.inode_number)),
    ]);
    match kind {
        FileType::Symlink => fields.push(("target", Json::from(show(&inode.symlink_target)))),
        FileType::CharDev | FileType::BlockDev => fields.extend([
            ("major", Json::from(inode.rdev_major())),
            ("minor", Json::from(inode.rdev_minor())),
        ]),
        _ => {}
    }
    Json::object(fields)
}

fn entry_text(e: &Json) -> String {
    let field = |k: &str| e.get(k).map(Json::to_text).unwrap_or_default();
    let mut line = format!(
        "{}{} {:>12} {}",
        type_char(&field("type")),
        field("mode"),
        field("size"),
        e.get("path")
            .map(Json::to_text)
            .unwrap_or_else(|| field("name"))
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    if e.get("revisited") == Some(&Json::Bool(true)) {
        line.push_str(" [revisited]");
    }
    line
}

fn ls(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| squashfs_error(path, e))?;
    let entries = if inode.is_dir() {
        let mut listed = Vec::new();
        for d in fs.read_dir(&inode).map_err(|e| squashfs_error(path, e))? {
            let child = fs.read_inode(d.inode_ref).map_err(|e| {
                let mut full = path.to_vec();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&d.name);
                squashfs_error(&full, e)
            })?;
            listed.push(entry(&d.name, &child));
        }
        listed
    } else {
        let name = path
            .rsplit(|b| *b == b'/')
            .find(|s| !s.is_empty())
            .unwrap_or(path);
        vec![entry(name, &inode)]
    };
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

fn recursive_entry(path: &[u8], depth: u64, inode: &Inode, revisited: bool) -> Json {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let Json::Obj(mut fields) = entry(name, inode) else {
        unreachable!()
    };
    fields.push(("path".into(), Json::from(show(path))));
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex".into(),
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("depth".into(), Json::from(depth)));
    if revisited {
        fields.push(("revisited".into(), Json::from(true)));
    }
    Json::Obj(fields)
}

/// The stack holds directory iterators, not Rust call frames, so even a
/// caller-supplied large depth bound cannot overflow the process stack.
fn ls_recursive(fs: &Filesystem, path: &[u8], sub: &ArgMatches) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| squashfs_error(path, e))?;
    let max_depth = sub.get_one::<u64>("max-depth").copied().unwrap_or(64);
    let limit = sub.get_one::<u64>("limit").copied().unwrap_or(100_000);
    let mut full = Vec::new();
    for component in path.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        full.push(b'/');
        full.extend_from_slice(component);
    }
    if full.is_empty() {
        full.push(b'/');
    }
    let mut entries = Vec::new();
    let result = (|| {
        if !inode.is_dir() {
            if limit == 0 {
                return Err(CliError::failed(format!(
                    "--limit 0 reached at {}",
                    show(&full)
                )));
            }
            entries.push(recursive_entry(&full, 0, &inode, false));
            return Ok(());
        }
        let children = |node: &Inode, at: &[u8]| {
            let mut children = fs.read_dir(node).map_err(|e| squashfs_error(at, e))?;
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok::<_, CliError>(children.into_iter())
        };
        let mut visited = HashSet::from([inode.inode_number]);
        let first = children(&inode, &full)?;
        let mut stack = vec![(full, 0u64, first)];
        while let Some((parent, parent_depth, children_iter)) = stack.last_mut() {
            let Some(child) = children_iter.next() else {
                stack.pop();
                continue;
            };
            let mut full = parent.clone();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(&child.name);
            let depth = *parent_depth + 1;
            if entries.len() as u64 >= limit {
                return Err(CliError::failed(format!(
                    "--limit {limit} reached at {}",
                    show(&full)
                )));
            }
            if depth > max_depth {
                return Err(CliError::failed(format!(
                    "--max-depth {max_depth} reached at {}",
                    show(&full)
                )));
            }
            let inode = fs
                .read_inode(child.inode_ref)
                .map_err(|e| squashfs_error(&full, e))?;
            let revisited = inode.is_dir() && !visited.insert(inode.inode_number);
            entries.push(recursive_entry(&full, depth, &inode, revisited));
            if inode.is_dir() && !revisited {
                let next = children(&inode, &full)?;
                stack.push((full, depth, next));
            }
        }
        Ok(())
    })();
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Outcome::report(Json::Arr(entries)).with_text(text);
    match result {
        Ok(()) => Ok(report),
        Err(error) => {
            // Outcome carries one stream; emit the partial report through
            // the shared renderer, then let the caller render the error.
            let format = fs_core::cli::Format::of(sub, &std::env::args_os().collect::<Vec<_>>());
            let partial = fs_core::cli::output::render("fs.squashfs", format, Ok(report));
            std::io::stdout()
                .lock()
                .write_all(partial.stdout.as_bytes())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            Err(error)
        }
    }
}

/// The shared stat fields, then the inode's SquashFS layout. Fragments
/// can be shared and compressed: no per-file allocation total is stored.
/// Neither that total nor unstored times or nanoseconds are invented.
fn stat(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| squashfs_error(path, e))?;
    let uid = fs
        .resolve_uid(inode.uid_idx)
        .map_err(|e| squashfs_error(path, e))?;
    let gid = fs
        .resolve_gid(inode.gid_idx)
        .map_err(|e| squashfs_error(path, e))?;
    let kind = inode.file_type();
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("inode", Json::from(inode.inode_number)),
        ("type", Json::from(type_name(kind))),
        (
            "mode",
            Json::from(format!("{:04o}", inode.permissions & 0o7777)),
        ),
        ("uid", Json::from(uid)),
        ("gid", Json::from(gid)),
        ("nlink", Json::from(inode.nlink)),
        ("size", Json::from(inode.file_size)),
        ("allocated_bytes", Json::Null),
        ("atime", Json::Null),
        ("atime_nsec", Json::Null),
        ("mtime", Json::from(i64::from(inode.mtime))),
        ("mtime_nsec", Json::Null),
        ("ctime", Json::Null),
        ("ctime_nsec", Json::Null),
        ("btime", Json::Null),
        ("btime_nsec", Json::Null),
    ]);
    match kind {
        FileType::Symlink => fields.push(("target", Json::from(show(&inode.symlink_target)))),
        FileType::CharDev | FileType::BlockDev => fields.extend([
            ("major", Json::from(inode.rdev_major())),
            ("minor", Json::from(inode.rdev_minor())),
        ]),
        _ => {}
    }
    fields.push((
        "squashfs",
        Json::object([
            ("inode_type", Json::from(inode.inode_type)),
            (
                "fragment_index",
                Json::from(inode.has_fragment().then_some(inode.fragment_index)),
            ),
            (
                "fragment_offset",
                Json::from(inode.has_fragment().then_some(inode.fragment_offset)),
            ),
            (
                "block_count",
                Json::from(
                    inode
                        .is_regular_file()
                        .then_some(inode.block_sizes.len() as u64),
                ),
            ),
            (
                "xattr_index",
                Json::from(inode.has_xattrs().then_some(inode.xattr_index)),
            ),
        ]),
    ));
    Ok(Outcome::report(Json::object(fields)))
}

/// Stream a regular file's bytes. Each chunk is read before it is
/// written, so a file whose blocks turn out unreadable part-way stops with
/// status 1; everything that can be refused up front (no such path, a
/// directory, a symlink, a device) is refused before a byte is written.
/// `-o FILE` writes `FILE.partial` and renames it, so FILE is never left
/// half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let inode = fs
        .lookup_path_bytes(path)
        .map_err(|e| squashfs_error(path, e))?;
    match inode.file_type() {
        FileType::RegFile => {}
        FileType::Dir => {
            return Err(CliError::failed(format!("{}: is a directory", show(path))));
        }
        FileType::Symlink => {
            return Err(CliError::failed(format!(
                "{}: is a symlink to {}; read the target instead",
                show(path),
                show(&inode.symlink_target)
            )));
        }
        _ => {
            return Err(CliError::failed(format!(
                "{}: not a regular file",
                show(path)
            )));
        }
    }
    const CHUNK: usize = 1 << 20;
    let size = inode.file_size;
    let mut buf = vec![0u8; CHUNK.min(size.try_into().unwrap_or(CHUNK))];
    let mut copy = |sink: &mut dyn Write| -> Result<(), CliError> {
        let mut offset = 0u64;
        while offset < size {
            let want = buf
                .len()
                .min((size - offset).try_into().unwrap_or(usize::MAX));
            let got = fs
                .read_file(&inode, offset, &mut buf[..want])
                .map_err(|e| squashfs_error(path, e))?;
            if got == 0 {
                return Err(CliError::failed(format!(
                    "{}: short read at byte {offset} of {size}",
                    show(path)
                )));
            }
            sink.write_all(&buf[..got])
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            offset += got as u64;
        }
        sink.flush()
            .map_err(|e| CliError::failed(format!("write: {e}")))
    };
    match output {
        None => copy(&mut std::io::stdout().lock())?,
        Some(file) => {
            let dest = std::path::Path::new(file);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut f = std::fs::File::create(&partial)
                .map_err(|e| CliError::failed(format!("create {}: {e}", partial.display())))?;
            if let Err(e) = copy(&mut f) {
                drop(f);
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
}

fn flag_names(bits: u16) -> Json {
    let mut names: Vec<Json> = FLAGS
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| Json::from(*name))
        .collect();
    let unknown = FLAGS.iter().fold(bits, |rest, (bit, _)| rest & !bit);
    if unknown != 0 {
        names.push(Json::from(format!("0x{unknown:04x}")));
    }
    Json::Arr(names)
}

/// The envelope: the shared keys first, SquashFS's own under `squashfs`.
/// The format has no label, so `label` is null; an image is written once
/// and never modified, so nothing is free and nothing is dirty.
pub fn envelope(fs: &Filesystem) -> Json {
    let sb = &fs.sb;
    Json::object([
        ("fs", Json::from("squashfs")),
        ("label", Json::Null),
        ("total_bytes", Json::from(sb.bytes_used)),
        ("free_bytes", Json::from(0u64)),
        ("block_size", Json::from(sb.block_size)),
        ("dirty", Json::from(false)),
        (
            "squashfs",
            Json::object([
                ("compression", Json::from(fs.compressor().name())),
                (
                    "version",
                    Json::from(format!("{}.{}", sb.version_major, sb.version_minor)),
                ),
                ("inode_count", Json::from(sb.inode_count)),
                ("fragment_count", Json::from(sb.fragment_entry_count)),
                ("id_count", Json::from(sb.id_count)),
                ("bytes_used", Json::from(sb.bytes_used)),
                ("mkfs_time", Json::from(sb.modification_time)),
                ("flags", flag_names(sb.flags)),
                ("exportable", Json::from(fs.is_exportable())),
            ]),
        ),
    ])
}

fn get(fs: &Filesystem, key: Option<&str>) -> Result<Outcome, CliError> {
    let all = envelope(fs);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and squashfs.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}
