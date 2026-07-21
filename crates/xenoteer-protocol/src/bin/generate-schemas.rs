//! Generates or checks deterministic protocol schemas.

use std::process::ExitCode;

fn main() -> ExitCode {
    let check = match parse_check_flag(std::env::args_os().skip(1)) {
        Ok(check) => check,
        Err(()) => {
            eprintln!("usage: generate-schemas [--check]");
            return ExitCode::from(2);
        }
    };
    let directory = xenoteer_protocol::schema::checked_in_schema_dir();
    match xenoteer_protocol::schema::write_or_check(&directory, check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("schema operation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_check_flag<I>(arguments: I) -> Result<bool, ()>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let arguments: Vec<_> = arguments.into_iter().collect();
    match arguments.as_slice() {
        [] => Ok(false),
        [argument] if argument == "--check" => Ok(true),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn accepts_only_no_argument_or_exact_check_flag() {
        assert_eq!(parse_check_flag(Vec::<OsString>::new()), Ok(false));
        assert_eq!(parse_check_flag([OsString::from("--check")]), Ok(true));
        assert_eq!(parse_check_flag([OsString::from("--chek")]), Err(()));
        assert_eq!(
            parse_check_flag([OsString::from("--check"), OsString::from("extra")]),
            Err(())
        );
    }
}
