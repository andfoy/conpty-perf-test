use std::env::temp_dir;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use pathsearch::find_executable_in_path;
use serde_json::json;
use winptyrs::{PTY, PTYArgs, PTYBackend};

#[derive(Debug, Default, Clone, ValueEnum)]
enum Producer {
    #[default]
    Type,
    Bat,
    Cat,
    GetContent,
    Python,
}

impl ToString for Producer {
    fn to_string(&self) -> String {
        match self {
            Producer::Type => "type",
            Producer::Bat => "bat",
            Producer::Cat => "cat",
            Producer::GetContent => "get_content",
            Producer::Python => "python",
        }
        .into()
    }
}

#[derive(Parser, Debug)]
#[command(
    version,
    about,
    long_about = "Measure ConPTY behavior for a large plain-text producer"
)]
struct Args {
    /// Number of lines to write in the test file
    #[arg(short, long, default_value_t = 200_000)]
    lines: i32,

    /// Producer executable used to read and output the test file contents
    #[arg(short, long, default_value_t)]
    producer: Producer,

    /// Number of PTY cols
    #[arg(short, long, default_value_t = 120)]
    cols: i32,

    /// Number of PTY rows
    #[arg(short, long, default_value_t = 40)]
    rows: i32,
}

#[derive(Debug, Default, serde::Serialize)]
struct Stats {
    #[serde(skip)]
    pub read_sizes: Vec<usize>,
    pub total_chars: usize,
    pub total_bytes: usize,
    pub drain_time_out: bool,
    pub exitstatus: Option<u32>,
    pub reached_eof: bool,
    pub elapsed_seconds: Duration,
    pub chars_per_second: f64,
    pub mb_per_second: f64,
    pub mean_chars_per_read: f64,
    pub max_chars_per_read: usize,
    pub median_chars_per_read: usize
}

impl Stats {
    fn compute_stats(&mut self) {
        self.chars_per_second = self.total_chars as f64 / self.elapsed_seconds.as_secs_f64();
        self.mb_per_second = (self.total_bytes as f64 / self.elapsed_seconds.as_secs_f64()) / 1024.0 / 1024.0;

        let sum: usize = self.read_sizes.iter().sum();
        let len = self.read_sizes.len();
        self.mean_chars_per_read = if len > 0 { sum as f64 / len as f64 } else { 0.0 };
        self.max_chars_per_read = self.read_sizes.iter().fold(0, |a, &b| a.max(b));

        let mut sorted_sizes = self.read_sizes.clone();
        sorted_sizes.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let mid = sorted_sizes.len() / 2;
        self.median_chars_per_read = if sorted_sizes.len() % 2 == 0 {
            (sorted_sizes[mid - 1] + sorted_sizes[mid]) / 2
        } else {
            sorted_sizes[mid]
        };
    }
}


fn create_file(path: PathBuf, num_lines: i32) {
    let mut left_block = String::from("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
    left_block.push_str("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");

    let right_block = &left_block[0..(left_block.len() - 1)];

    let f = File::create(path).unwrap();
    let mut writer = BufWriter::new(f);

    writeln!(&mut writer, "WinPTY-rs large output throughout fixture").unwrap();
    writeln!(&mut writer, "lines={}", num_lines).unwrap();

    for i in 1..num_lines + 1 {
        writeln!(&mut writer, "{} | {} | {}", i, left_block, right_block).unwrap();
    }

    writer.flush().unwrap();
}

fn build_command(producer: Producer, path: PathBuf) -> (OsString, OsString) {
    match producer {
        Producer::Type => {
            // "/d", "/c", "type", str(fixture_path)
            let cmd_loc = find_executable_in_path("cmd").unwrap();
            let args = format!("/d /c type {}", path.to_str().unwrap());
            (
                OsString::from(cmd_loc.to_str().unwrap()),
                OsString::from(args),
            )
        }
        Producer::Bat => {
            let cmd_loc = find_executable_in_path("bat").unwrap();
            let args = format!("-P {}", path.to_str().unwrap());
            (
                OsString::from(cmd_loc.to_str().unwrap()),
                OsString::from(args),
            )
        }
        Producer::Cat => {
            let cmd_loc = find_executable_in_path("cat").unwrap();
            let args = format!("{}", path.to_str().unwrap());
            (
                OsString::from(cmd_loc.to_str().unwrap()),
                OsString::from(args),
            )
        }
        Producer::GetContent => {
            let cmd_loc = find_executable_in_path("pwsh").unwrap();
            let args = format!(
                "-NoProfile -Command Get-Content -Path '{}'",
                path.to_str().unwrap()
            );
            (
                OsString::from(cmd_loc.to_str().unwrap()),
                OsString::from(args),
            )
        },
        Producer::Python => {
            let script = r#"
import shutil
import sys

with open(sys.argv[1], "rb") as f:
    shutil.copyfileobj(f, sys.stdout.buffer)
            "#;

            let python = find_executable_in_path("python").unwrap();
            let args = format!("-u -c {:?} {}", script,  path.to_str().unwrap());
            (
                OsString::from(python.to_str().unwrap()),
                OsString::from(args),
            )
        }
    }
}

fn accumulate_output(output: OsString, stats: &mut Stats) {
    stats.read_sizes.push(output.len());
    stats.total_chars += output.len();
    stats.total_bytes += output.as_encoded_bytes().len();
}

fn drain_after_exit(pty: &mut PTY, stats: &mut Stats) {
    let deadline = Duration::from_secs(2);
    let mut start = Instant::now();

    loop {
        match pty.is_eof() {
            Ok(true) => {
                stats.reached_eof = true;
                stats.drain_time_out = false;
                break;
            },
            Ok(false) => {

            },
            Err(err) => panic!("{:?}", err)
        }

        match pty.read(false) {
            Ok(out) => {
                match out.is_empty() {
                    true => {
                        if Instant::now() - start >= deadline {
                            stats.reached_eof = false;
                            stats.drain_time_out = true;
                            break;
                        }
                        sleep(Duration::from_millis(10));
                    },
                    false => {
                        // println!("{:?}", out);
                        accumulate_output(out, stats);
                        start = Instant::now();
                        continue;
                    }
                }
            },
            Err(_) => {
                match pty.is_eof() {
                    Ok(true) => {
                        stats.reached_eof = true;
                        stats.drain_time_out = false;
                        break;
                    },
                    Ok(false) => {
                        if Instant::now() - start >= deadline {
                            stats.reached_eof = false;
                            stats.drain_time_out = true;
                            break;
                        }
                        sleep(Duration::from_millis(10));
                        continue;
                    },
                    Err(err) => {
                        panic!("{:?}", err);
                    }
                }
            }
        }
    }

}

fn measure_pty(appname: OsString, cmdline: Option<OsString>, cols: i32, rows: i32, stats: &mut Stats) {
    let mut pty_args = PTYArgs::default();
    pty_args.cols = cols;
    pty_args.rows = rows;

    let mut pty = PTY::new_with_backend(&pty_args, PTYBackend::ConPTY).unwrap();
    pty.spawn(appname, cmdline, None, None).unwrap();
    pty.write(OsString::from("\x1b[?1;0c\x1b[0;0R")).unwrap();

    let start = Instant::now();

    loop {
        let output = pty.read(true);
        match output {
            Ok(out) => {
                // println!("{:?}", out);
                match out.is_empty() {
                    true => {
                        if pty.is_eof().unwrap() {
                            stats.reached_eof = true;
                            break;
                        }

                        let exitstatus = pty.get_exitstatus().unwrap();
                        match exitstatus {
                            Some(_) => {
                                drain_after_exit(&mut pty, stats);
                                break;
                            },
                            None => {
                                continue;
                            }
                        }
                    },
                    false => {
                        accumulate_output(out, stats);
                    }
                }
            },
            Err(err) => {
                stats.exitstatus = pty.get_exitstatus().unwrap();
                match (pty.is_eof(), stats.exitstatus) {
                    (Ok(true), _) => {
                        stats.reached_eof = true;
                        break;
                    },
                    (Ok(false), Some(_)) => {

                    },
                    (Ok(false), None) => {
                        break;
                    }
                    (Err(_), _) => {
                        panic!("{:?}", err);
                    }
                }

            }
        }
    }

    stats.elapsed_seconds = Instant::now() - start;

}

fn main() {
    println!("Hello, world!");
    let args = Args::parse();

    let tmp_dir = temp_dir();
    let tmp_file = tmp_dir.join("large-output-file");
    let tmp_clone = tmp_file.clone();
    create_file(tmp_file, args.lines);

    let mut stats = Stats::default();
    let (command, cmd_args) = build_command(args.producer, tmp_clone);
    measure_pty(command, Some(cmd_args), args.cols, args.rows, &mut stats);

    stats.compute_stats();
    println!("{}", json!(stats).to_string());
}
