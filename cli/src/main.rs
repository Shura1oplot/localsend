use localsend_cli::{Command, parse_args, send};

#[tokio::main]
async fn main() {
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Ошибка: {error:#}");
            std::process::exit(2);
        }
    };

    match command {
        Command::Help => println!("{}", localsend_cli::USAGE),
        Command::Send(input) => {
            if let Err(error) = send(input).await {
                eprintln!("Ошибка: {error:#}");
                std::process::exit(1);
            }
            println!("Передача завершена.");
        }
    }
}
