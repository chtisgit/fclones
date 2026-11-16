use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs::{create_dir_all, remove_dir_all, File, OpenOptions};
use std::io;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::character::complete::{none_of, one_of};
use nom::combinator::map;
use nom::error::{ErrorKind, ParseError};
use nom::multi::{many1, separated_list0};
use nom::sequence::tuple;
use nom::IResult;
use regex::Regex;
use uuid::Uuid;

use crate::path::Path;

/// Controls how we pass data to the child process.
/// By default, the file to process is sent to the standard input of the child process.
/// Some programs do not accept reading input from the stdin, but prefer to be pointed
/// to a file by a command-line option - in this case `Named` variant is used.
enum Input {
    /// Pipe the input file from the given path to the stdin of the child
    StdIn(PathBuf),
    /// Pass the original path to the file as $IN param
    Named(PathBuf),
    /// Copy the original file to a temporary location and pass it as $IN param
    Copied(PathBuf, PathBuf),
}

impl Input {
    fn input_path(&self) -> &PathBuf {
        match self {
            Input::StdIn(path) => path,
            Input::Named(path) => path,
            Input::Copied(_src, target) => target,
        }
    }

    fn prepare_input_file(&self) -> io::Result<()> {
        match self {
            Input::StdIn(_path) => Ok(()),
            Input::Named(_path) => Ok(()),
            Input::Copied(src, target) => {
                std::fs::copy(src, target)?;
                Ok(())
            }
        }
    }
}

impl Drop for Input {
    /// Removes the temporary file if it was created
    fn drop(&mut self) {
        let _ = match self {
            Input::StdIn(_) => Ok(()),
            Input::Named(_) => Ok(()),
            Input::Copied(_, target) => std::fs::remove_file(target),
        };
    }
}

/// Controls how we read data out from the child process.
/// By default we read output directly from the standard output of the child process.
/// If the preprocessor program can't output data to its stdout, but supports only writing
/// to files, it can be configured to write to a named pipe, and we read from that named pipe.
enum Output {
    /// Pipe data directly to StdOut
    StdOut,
    /// Send data through a named pipe
    Named(PathBuf),
    /// Read data from the same file as the input
    InPlace(PathBuf),
}

impl Output {
    /// Returns the path to the named pipe if the process is configured to write to a pipe.
    /// Returns None if the process is configured to write to stdout or to modify the input file.
    pub fn pipe_path(&self) -> Option<PathBuf> {
        match &self {
            Output::Named(output) => Some(output.clone()),
            _ => None,
        }
    }
}

impl Drop for Output {
    /// Removes the output file if it was created
    fn drop(&mut self) {
        let _ = match self {
            Output::StdOut => Ok(()),
            Output::Named(target) => std::fs::remove_file(target),
            // The in-place output is the input file itself: either the temporary copy, which
            // is removed by `Input::Copied`, or - with `--no-copy` - the user's original file,
            // which must never be deleted.
            Output::InPlace(_) => Ok(()),
        };
    }
}

/// Transforms files through an external program.
/// The `command_str` field contains a path to a program and its space separated arguments.
/// The command takes a file given in the `$IN` variable and produces an `$OUT` file.
#[derive(Clone)]
pub struct Transform {
    /// a path to a program and its space separated arguments
    pub command_str: String,
    /// temporary directory for storing files and named pipes
    pub tmp_dir: PathBuf,
    /// copy the file into temporary directory before running the transform on it
    pub copy: bool,
    /// read output from the same location as the original
    pub in_place: bool,
    /// will be set to the name of the program, extracted from the command_str
    pub program: String,
}

impl Transform {
    pub fn new(command_str: String, in_place: bool) -> io::Result<Transform> {
        let has_in = RefCell::new(false);
        let has_out = RefCell::new(false);

        let parsed = parse_command(&command_str, |s: &str| {
            match s {
                "OUT" if cfg!(windows) => *has_out.borrow_mut() = true,
                "IN" => *has_in.borrow_mut() = true,
                _ => {}
            };
            OsString::from(format!("${s}"))
        })?;

        let has_in = has_in.into_inner();
        let has_out = has_out.into_inner();

        if cfg!(windows) && has_out {
            return Err(io::Error::other("$OUT not supported on Windows yet"));
        }
        if in_place && has_out {
            return Err(io::Error::other("$OUT conflicts with --in-place"));
        }
        if in_place && !has_in {
            return Err(io::Error::other("$IN required with --in-place"));
        }

        let program_path = match parsed.first() {
            Some(p) => p,
            None => return Err(io::Error::other("Command cannot be empty")),
        };
        let program = PathBuf::from(program_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| program_path.to_string_lossy().to_string());

        // Check that the program can be found, fail fast if it can't. The program is looked up
        // the same way it will be launched (a path with a separator is used as given, a bare
        // name is searched in PATH), but it is not executed here.
        if find_executable(program_path).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Cannot launch {}: program not found or not executable",
                    program_path.to_string_lossy()
                ),
            ));
        }

        Ok(Transform {
            command_str,
            program,
            tmp_dir: Transform::create_temp_dir()?,
            copy: has_in,
            in_place,
        })
    }

    /// Creates the directory where preprocessed files will be stored
    fn create_temp_dir() -> io::Result<PathBuf> {
        let tmp = std::env::temp_dir().join(format!("fclones-{:032x}", Uuid::new_v4().as_u128()));
        match create_dir_all(&tmp) {
            Ok(()) => Ok(tmp),
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!(
                    "Failed to create temporary directory {}: {}",
                    tmp.display(),
                    e
                ),
            )),
        }
    }

    /// Creates a new unique random file name in the temporary directory
    fn random_tmp_file_name(&self) -> PathBuf {
        self.tmp_dir
            .join(format!("{:032x}", Uuid::new_v4().as_u128()))
    }

    /// Returns the output file path for the given input file path
    pub fn output(&self, input: &Path) -> PathBuf {
        self.tmp_dir.join(format!("{:x}", input.hash128()))
    }

    /// Processes the input file and returns its output and err as stream
    pub fn run(&self, input: &Path) -> io::Result<Execution> {
        let (args, input_conf, output_conf) = self.make_args(input)?;
        let mut command = build_command(&args, &input_conf, &output_conf)?;
        let result = execute(&mut command, input_conf, output_conf)?;
        Ok(result)
    }

    /// Creates arguments, input and output configuration for processing given input path.
    /// The first element of the argument vector contains the program name.
    fn make_args(&self, input: &Path) -> io::Result<(Vec<OsString>, Input, Output)> {
        let input_conf = RefCell::<Input>::new(Input::StdIn(input.to_path_buf()));
        let output_conf = RefCell::<Output>::new(Output::StdOut);

        let args = parse_command(self.command_str.as_str(), |arg| match arg {
            "IN" if self.copy => {
                let tmp_target = self.random_tmp_file_name();
                input_conf.replace(Input::Copied(input.to_path_buf(), tmp_target.clone()));
                tmp_target.into_os_string()
            }
            "IN" => {
                let input = input.to_path_buf();
                input_conf.replace(Input::Named(input.clone()));
                input.into_os_string()
            }
            "OUT" => {
                let output = self.output(input);
                output_conf.replace(Output::Named(output.clone()));
                output.into_os_string()
            }
            _ => OsString::from(format!("${arg}")),
        })?;

        let input_conf = input_conf.into_inner();
        let mut output_conf = output_conf.into_inner();

        if self.in_place {
            output_conf = Output::InPlace(input_conf.input_path().clone())
        }

        Ok((args, input_conf, output_conf))
    }
}

/// Cleans up temporary files
impl Drop for Transform {
    fn drop(&mut self) {
        let _ = remove_dir_all(&self.tmp_dir);
    }
}

/// Keeps the results of the transform program execution
pub struct Execution {
    pub(crate) child: Arc<Mutex<Child>>,
    pub(crate) out_stream: Box<dyn Read>,
    pub(crate) err_stream: Option<JoinHandle<String>>,
    _input: Input,   // holds the temporary input file(s) until execution is done
    _output: Output, // holds the temporary output file(s) until execution is done
}

impl Drop for Execution {
    fn drop(&mut self) {
        let mut buf = [0; 4096];
        while let Ok(1..) = self.out_stream.read(&mut buf) {}
        let _ = self.child.lock().unwrap().wait();
    }
}

/// Finds the executable that `Command::new(program)` would launch, without launching it:
/// a path containing a directory separator is used as given, a bare name is searched in `PATH`
/// (on Windows also with the extensions from `PATHEXT`).
fn find_executable(program: &OsStr) -> Option<PathBuf> {
    #[cfg(unix)]
    fn is_executable(path: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    fn is_executable(path: &std::path::Path) -> bool {
        path.is_file()
    }
    fn candidates(path: PathBuf) -> Vec<PathBuf> {
        let mut result = vec![path.clone()];
        if cfg!(windows) && path.extension().is_none() {
            let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            for ext in exts.split(';').filter(|e| !e.is_empty()) {
                let mut p = path.clone().into_os_string();
                p.push(ext);
                result.push(PathBuf::from(p));
            }
        }
        result
    }

    let path = std::path::Path::new(program);
    if path.components().count() > 1 {
        return candidates(path.to_path_buf())
            .into_iter()
            .find(|p| is_executable(p));
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .flat_map(|dir| candidates(dir.join(program)))
        .find(|p| is_executable(p))
}

/// Builds the `Command` struct from the parsed arguments
fn build_command(
    args: &[OsString],
    input_conf: &Input,
    output_conf: &Output,
) -> io::Result<Command> {
    let mut args = args.iter();
    let mut command = Command::new(args.next().unwrap());
    command.args(args);
    command.stderr(Stdio::piped());

    input_conf.prepare_input_file()?;
    if let Input::StdIn(_) = input_conf {
        command.stdin(File::open(input_conf.input_path())?);
    } else {
        command.stdin(Stdio::null());
    }

    if let Output::Named(output) = output_conf {
        command.stdout(Stdio::null());
        create_named_pipe(output)?;
    } else {
        command.stdout(Stdio::piped());
    }

    Ok(command)
}

#[cfg(unix)]
fn create_named_pipe(path: &std::path::Path) -> io::Result<()> {
    use nix::sys::stat;
    use nix::unistd::mkfifo;
    if let Err(e) = mkfifo(path, stat::Mode::S_IRWXU) {
        let io_err: io::Error = e.into();
        return Err(io::Error::new(
            io_err.kind(),
            format!("Failed to create named pipe {}: {}", path.display(), io_err),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn create_named_pipe(_path: &PathBuf) -> io::Result<()> {
    unimplemented!()
}

/// Spawns the command process, and returns its output as a stream.
/// The standard error is captured by a background thread and read to a string.
fn execute(command: &mut Command, input: Input, output: Output) -> io::Result<Execution> {
    let child = Arc::new(Mutex::new(command.spawn()?));

    // We call 'take' to avoid borrowing `child` for longer than a single line.
    // We can't reference stdout/stderr directly, because a mutable borrow of a field
    // creates a mutable borrow of the containing struct, but later we need to mutably
    // borrow `child` again to wait on it.
    let child_out = child.lock().unwrap().stdout.take();
    let child_err = child.lock().unwrap().stderr.take();

    let output_pipe = output.pipe_path();
    let child_ref = child.clone();

    // Capture the stderr in background in order to avoid a deadlock when the child process
    // would block on writing to stdout, and this process would block on reading stderr
    // (or the other way round).
    // The other solution could be to use non-blocking I/O, but threads look simpler.
    let stderr_reaper = std::thread::spawn(move || {
        let mut str = String::new();
        if let Some(mut stream) = child_err {
            let _ = stream.read_to_string(&mut str);
        }
        // If the child is supposed to communicate its output through a named pipe,
        // ensure the pipe gets closed and the reader at the other end receives an EOF.
        // It is possible that due to a misconfiguration
        // (e.g. wrong arguments given by the user) the child would never open the output file
        // and the reader at the other end would block forever.
        if let Some(output_pipe) = output_pipe {
            // If those fail, we have no way to report the failure.
            // However if waiting fails here, the child process likely doesn't run, so that's not
            // a problem.
            let _ignore = child_ref.lock().unwrap().wait();
            let _ignore = OpenOptions::new().write(true).open(output_pipe);
        }
        str
    });

    let child_out: Box<dyn Read> = match &output {
        Output::StdOut => Box::new(child_out.unwrap()),
        Output::Named(output) => Box::new(File::open(output)?),
        Output::InPlace(output) => {
            child.lock().unwrap().wait()?;
            Box::new(File::open(output)?)
        }
    };

    Ok(Execution {
        child,
        out_stream: child_out,
        err_stream: Some(stderr_reaper),
        _input: input,
        _output: output,
    })
}

/// Compares the input with a regular expression and returns the first match.
/// Backported from nom 6.0.0-alpha1. We can't use nom 6.0.0-alpha1 directly,
/// because it had some issues with our use of functions in pattern.rs.
fn re_find<'s, E>(re: Regex) -> impl Fn(&'s str) -> IResult<&'s str, &'s str, E>
where
    E: ParseError<&'s str>,
{
    move |i| {
        if let Some(m) = re.find(i) {
            Ok((&i[m.end()..], &i[m.start()..m.end()]))
        } else {
            Err(nom::Err::Error(E::from_error_kind(
                i,
                ErrorKind::RegexpMatch,
            )))
        }
    }
}

/// Splits the command string into separate arguments and substitutes $params
/// Returns an error if the command cannot be parsed completely.
/// A `$` that does not start a variable name is kept literally (e.g. `sed s/a$/b/`).
fn parse_command<F>(command: &str, substitute: F) -> io::Result<Vec<OsString>>
where
    F: Fn(&str) -> OsString,
{
    fn join_chars(chars: Vec<char>) -> OsString {
        let mut result = OsString::new();
        for c in chars {
            result.push(c.to_string())
        }
        result
    }

    fn join_str(strings: Vec<OsString>) -> OsString {
        let mut result = OsString::new();
        for c in strings {
            result.push(c)
        }
        result
    }

    let r_var = Regex::new(r"^([[:alnum:]]|_)+").unwrap();
    let p_var = map(tuple((tag("$"), re_find(r_var))), |(_, str)| {
        (substitute)(str)
    });
    let p_non_var = map(many1(none_of(" \t$")), join_chars);
    let p_dollar = map(tag("$"), |_| OsString::from("$"));
    let p_arg = map(many1(alt((p_var, p_non_var, p_dollar))), join_str);
    let p_whitespace = many1(one_of(" \t"));
    let p_args = |s| separated_list0(p_whitespace, p_arg)(s);
    let command = command.trim_matches(|c| c == ' ' || c == '\t');
    let result: IResult<&str, Vec<OsString>> = (p_args)(command);
    match result {
        Ok(("", args)) => Ok(args),
        Ok((rest, _)) => Err(io::Error::other(format!(
            "Cannot parse command '{command}' at '{rest}'"
        ))),
        Err(e) => Err(io::Error::other(format!(
            "Cannot parse command '{command}': {e}"
        ))),
    }
}

#[cfg(test)]
mod test {
    use std::io::Write;

    use crate::file::{FileChunk, FileLen, FilePos};
    use crate::hasher::{FileHasher, HashFn};
    use crate::log::StdLog;
    use crate::util::test::with_dir;

    use super::*;

    #[test]
    fn empty() {
        assert!(Transform::new(String::from(" "), false).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn piped() {
        with_dir("target/test/transform/piped/", |root| {
            let transform = Transform::new(String::from("dd"), false).unwrap();
            let input_path = root.join("input.txt");
            let mut input = File::create(&input_path).unwrap();
            let content = b"content";
            input.write_all(content).unwrap();
            drop(input);

            let log = StdLog::default();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let input_path = Path::from(input_path);
            let chunk = FileChunk::new(&input_path, FilePos(0), FileLen::MAX);
            let good_file_hash = hasher.hash_file(&chunk, |_| {}).unwrap();
            let result = hasher.hash_transformed(&chunk, |_| {}).unwrap();
            assert_eq!(result.0, FileLen(content.len() as u64));
            assert_eq!(result.1, good_file_hash);
        })
    }

    #[test]
    #[cfg(unix)]
    fn parameterized() {
        with_dir("target/test/transform/param/", |root| {
            let transform = Transform::new(String::from("dd if=$IN of=$OUT"), false).unwrap();
            let input_path = root.join("input.txt");
            let mut input = File::create(&input_path).unwrap();
            let content = b"content";
            input.write_all(content).unwrap();
            drop(input);

            let log = StdLog::default();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let input_path = Path::from(input_path);

            let chunk = FileChunk::new(&input_path, FilePos(0), FileLen::MAX);
            let good_file_hash = hasher.hash_file(&chunk, |_| {}).unwrap();
            let result = hasher.hash_transformed(&chunk, |_| {}).unwrap();
            assert_eq!(result.0, FileLen(content.len() as u64));
            assert_eq!(result.1, good_file_hash);
        })
    }

    /// The whole transformed output must be hashed, not only as many bytes as the input file
    /// has: `od` output is ~4x longer than its input, and these two inputs only differ in the
    /// last byte, which appears in the output after the first `input.len()` bytes.
    #[test]
    #[cfg(unix)]
    fn hashes_whole_output_of_expanding_transform() {
        with_dir("target/test/transform/expanding/", |root| {
            let log = StdLog::default();
            let transform = Transform::new(String::from("od"), false).unwrap();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let hash = |name: &str, content: &[u8]| {
                let path = root.join(name);
                std::fs::write(&path, content).unwrap();
                let path = Path::from(&path);
                let chunk = FileChunk::new(&path, FilePos(0), FileLen(content.len() as u64));
                hasher.hash_transformed(&chunk, |_| {}).unwrap()
            };
            let (len_x, hash_x) = hash("x", b"aaaaaaaaaaaaaaaaX");
            let (_, hash_y) = hash("y", b"aaaaaaaaaaaaaaaaY");
            assert!(len_x.0 > 17, "the whole od output must be read");
            assert_ne!(hash_x, hash_y);
        })
    }

    /// A transform whose output exceeds the input length by more than the pipe buffer used to
    /// deadlock: fclones stopped reading and waited for the child, which blocked on write().
    #[test]
    #[cfg(unix)]
    fn large_expanding_transform_does_not_deadlock() {
        with_dir("target/test/transform/expanding_large/", |root| {
            let log = StdLog::default();
            let transform = Transform::new(String::from("od -v"), false).unwrap();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let path = root.join("big");
            std::fs::write(&path, vec![7u8; 200_000]).unwrap();
            let path = Path::from(&path);
            let chunk = FileChunk::new(&path, FilePos(0), FileLen(200_000));
            let (len, _) = hasher.hash_transformed(&chunk, |_| {}).unwrap();
            assert!(len.0 > 200_000);
        })
    }

    /// `--in-place --no-copy` lets the transform work on the original file. fclones must read
    /// the result but never delete the original (it used to delete every scanned file).
    #[test]
    #[cfg(unix)]
    fn in_place_without_copy_keeps_original_file() {
        with_dir("target/test/transform/in_place_no_copy/", |root| {
            let mut transform = Transform::new(String::from("touch $IN"), true).unwrap();
            transform.copy = false;
            let input_path = root.join("input.txt");
            std::fs::write(&input_path, b"content").unwrap();

            let log = StdLog::default();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let path = Path::from(&input_path);
            let chunk = FileChunk::new(&path, FilePos(0), FileLen(7));
            hasher.hash_transformed(&chunk, |_| {}).unwrap();
            assert_eq!(std::fs::read(&input_path).unwrap(), b"content");
        })
    }

    /// With the default copy, the temporary copy is removed and the original is kept.
    #[test]
    #[cfg(unix)]
    fn in_place_with_copy_removes_only_the_temporary_copy() {
        with_dir("target/test/transform/in_place_copy/", |root| {
            let transform = Transform::new(String::from("touch $IN"), true).unwrap();
            let tmp_dir = transform.tmp_dir.clone();
            let input_path = root.join("input.txt");
            std::fs::write(&input_path, b"content").unwrap();

            let log = StdLog::default();
            let hasher = FileHasher::new(HashFn::default(), Some(transform), &log);
            let path = Path::from(&input_path);
            let chunk = FileChunk::new(&path, FilePos(0), FileLen(7));
            hasher.hash_transformed(&chunk, |_| {}).unwrap();
            assert!(input_path.exists());
            assert_eq!(std::fs::read_dir(&tmp_dir).unwrap().count(), 0);
        })
    }

    #[test]
    fn parse_command() {
        let result = super::parse_command("foo bar", |s| OsString::from(s)).unwrap();
        assert_eq!(result, vec![OsString::from("foo"), OsString::from("bar")])
    }

    /// A `$` that does not start a variable name used to end parsing silently, so
    /// `sed -e s/one$/two/` ran as `sed -e s/one`. Unknown `$NAME`s lost their `$`.
    #[test]
    fn parse_command_keeps_literal_dollars() {
        let keep = |s: &str| OsString::from(format!("${s}"));
        assert_eq!(
            super::parse_command("sed -e s/one$/two/", keep).unwrap(),
            vec!["sed", "-e", "s/one$/two/"]
        );
        assert_eq!(
            super::parse_command("  echo $HOME $ end  ", keep).unwrap(),
            vec!["echo", "$HOME", "$", "end"]
        );
    }

    /// The pre-flight check used only the file name of the program and executed it.
    /// A program given by a path outside PATH must be accepted; a missing one rejected.
    #[test]
    #[cfg(unix)]
    fn transform_accepts_program_path_outside_path() {
        use std::os::unix::fs::PermissionsExt;
        with_dir("target/test/transform/program_path/", |root| {
            let script = root.join("my-transform");
            std::fs::write(&script, "#!/bin/sh\ncat\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let command = script.to_string_lossy().to_string();
            assert!(Transform::new(command, false).is_ok());

            let missing = root.join("missing").to_string_lossy().to_string();
            assert!(Transform::new(missing, false).is_err());
            assert!(Transform::new("no-such-program-fclones".to_string(), false).is_err());
        })
    }

    #[test]
    fn parse_command_substitute() {
        let result = super::parse_command("foo bar in=$IN", |s| match s {
            "IN" => OsString::from("/input"),
            _ => OsString::from(s),
        })
        .unwrap();

        assert_eq!(
            result,
            vec![
                OsString::from("foo"),
                OsString::from("bar"),
                OsString::from("in=/input")
            ]
        )
    }
}
