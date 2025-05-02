use std::env;
use std::io::{self, BufRead, Write};
use std::os::unix::io::RawFd;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::exit;

use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::sys::wait::waitpid;
use nix::unistd::{
    chdir, close, dup2, execvp, fork, pipe, ForkResult,
};

use std::ffi::CString;

#[derive(Debug)]
struct Command {
    is_background: bool,
    input_file: Option<String>,
    output_file: Option<String>,
    pipeline: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    loop {
        let current_dir = env::current_dir()?;
        print!("{}$ ", current_dir.display());
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "exit" {
            break;
        }

        if input.starts_with("cd ") {
            let parts: Vec<&str> = input.splitn(2, ' ').collect();
            if parts.len() > 1 {
                let path = parts[1];
                if let Err(e) = chdir(Path::new(path)) {
                    eprintln!("cd: {}: {}", path, e);
                }
            }
            continue;
        }

        match parse_command(input) {
            Ok(command) => {
                if let Err(e) = execute_command(command) {
                    eprintln!("Error executing command: {}", e);
                }
            }
            Err(e) => {
                eprintln!("Error parsing command: {}", e);
            }
        }
    }

    Ok(())
}

fn parse_command(input: &str) -> anyhow::Result<Command> {
    let mut command = Command {
        is_background: false,
        input_file: None,
        output_file: None,
        pipeline: Vec::new(),
    };

    let mut processed_input = input.to_string();
    if processed_input.ends_with(" &") {
        command.is_background = true;
        processed_input = processed_input[..processed_input.len() - 2].trim().to_string();
    }

    let pipeline_parts: Vec<&str> = processed_input.split('|').collect();
    
    for (i, part) in pipeline_parts.iter().enumerate() {
        let mut part_str = part.trim().to_string();
        
        if i == 0 && part_str.contains('<') {
            let parts: Vec<&str> = part_str.split('<').collect();
            let cmd = parts[0].trim().to_string();
            let file = parts[1].trim().to_string();
            
            part_str = cmd;
            command.input_file = Some(file);
        } 
        
        if i == pipeline_parts.len() - 1 && part_str.contains('>') {
            let parts: Vec<&str> = part_str.split('>').collect();
            let cmd = parts[0].trim().to_string();
            let file = parts[1].trim().to_string();
            
            part_str = cmd;
            command.output_file = Some(file);
        } 
        
        if !part_str.trim().is_empty() {
            command.pipeline.push(part_str);
        }
    }

    Ok(command)
}

fn execute_command(command: Command) -> anyhow::Result<()> {
    if command.pipeline.len() == 1 {
        return execute_simple_command(&command);
    }

    execute_pipeline(command)
}

fn execute_simple_command(command: &Command) -> anyhow::Result<()> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            if !command.is_background {
                waitpid(child, None)?;
            } else {
                println!("Starting background process {}", child);
            }
        }
        ForkResult::Child => {
            setup_redirection(command)?;
            let cmd = &command.pipeline[0];
            let args = externalize(cmd);
            
            execvp(&args[0], &args)?;
            
            eprintln!("Failed to execute: {}", cmd);
            exit(1);
        }
    }

    Ok(())
}

fn setup_redirection(command: &Command) -> anyhow::Result<()> {
    if let Some(input_file) = &command.input_file {
        let fd = open(
            Path::new(input_file),
            OFlag::O_RDONLY,
            Mode::empty(),
        )?;
        dup2(fd, 0)?;
        close(fd)?;
    }

    if let Some(output_file) = &command.output_file {
        let fd = open(
            Path::new(output_file),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
            Mode::from_bits_truncate(0o644),
        )?;
        dup2(fd, 1)?;
        close(fd)?;
    }

    Ok(())
}

fn execute_pipeline(command: Command) -> anyhow::Result<()> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            if !command.is_background {
                waitpid(child, None)?;
            } else {
                println!("Starting background process {}", child);
            }
        }
        ForkResult::Child => {
            match setup_pipeline(&command) {
                Ok(_) => {},
                Err(e) => {
                    eprintln!("Pipeline setup failed: {}", e);
                }
            }
            exit(1);
        }
    }

    Ok(())
}

//I used LLMs to help me make and debug this part
fn setup_pipeline(command: &Command) -> anyhow::Result<()> {
    let num_commands = command.pipeline.len();
    
    if num_commands == 1 {
        setup_redirection(command)?;
        let args = externalize(&command.pipeline[0]);
        execvp(&args[0], &args)?;
        return Err(anyhow::anyhow!("Failed to execute: {}", command.pipeline[0]));
    }
    
    let mut pipes = Vec::new();
    for _ in 0..num_commands - 1 {
        pipes.push(pipe()?);
    }
    
    let mut child_pids = Vec::new();
    
    for i in 0..num_commands {
        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                child_pids.push(child);
            },
            ForkResult::Child => {
                if i == 0 {
                    if let Some(input_file) = &command.input_file {
                        let fd = open(
                            Path::new(input_file),
                            OFlag::O_RDONLY,
                            Mode::empty(),
                        )?;
                        dup2(fd, 0)?;
                        close(fd)?;
                    }
                } else {
                    dup2(pipes[i-1].0.as_raw_fd(), 0)?;
                }
                
                if i == num_commands - 1 {
                    if let Some(output_file) = &command.output_file {
                        let fd = open(
                            Path::new(output_file),
                            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
                            Mode::from_bits_truncate(0o644),
                        )?;
                        dup2(fd, 1)?;
                        close(fd)?;
                    }
                } else {
                    dup2(pipes[i].1.as_raw_fd(), 1)?;
                }
                
                for (read_fd, write_fd) in &pipes {
                    close(read_fd.as_raw_fd())?;
                    close(write_fd.as_raw_fd())?;
                }
                
                let args = externalize(&command.pipeline[i]);
                execvp(&args[0], &args)?;
                
                eprintln!("Failed to execute: {}", command.pipeline[i]);
                exit(1);
            }
        }
    }
    
    for (read_fd, write_fd) in pipes {
        close(read_fd.as_raw_fd())?;
        close(write_fd.as_raw_fd())?;
    }
    
    for child_pid in child_pids {
        waitpid(child_pid, None)?;
    }
    
    exit(0);
}

fn externalize(command: &str) -> Vec<CString> {
    command.split_whitespace()
        .map(|s| CString::new(s).unwrap())
        .collect()
}