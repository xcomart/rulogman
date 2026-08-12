//! The WSL distributions installed on this machine.
//!
//! Windows has no single local shell, so the welcome screen offers one button
//! per shell it can start — and the interesting ones are not fixed: every
//! installed distribution is another shell the user may well have meant. There
//! is no supported API for the list, so it comes from `wsl.exe -l -q`, the
//! quiet form of the listing that prints one name per line and nothing else.
//!
//! [`list_distros`] runs a process and blocks, so the caller hands it to the
//! background executor rather than calling it while laying out a frame.

use std::os::windows::process::CommandExt;
use std::process::Command;

/// `CREATE_NO_WINDOW`, from the Win32 process creation flags.
///
/// Without it a GUI process starting a console program flashes a console
/// window on screen, which — for a listing the user never asked for and only
/// sees the result of — would be a black rectangle appearing over the welcome
/// screen at startup. Spelled out rather than pulled from the `windows` crate
/// so that this module needs no bindings at all.
///
/// Shared with the WSL file source, which starts `wsl.exe` for a reason the
/// user did not ask about either — and would flash the same rectangle over the
/// file panel every time it did.
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Distributions `wsl.exe` lists that no user opens a shell in.
///
/// Docker Desktop registers its engine and its data volume as distributions.
/// They are plumbing rather than a place to work — the data one has no shell
/// worth speaking of — so offering them would only be two buttons that lead
/// nowhere the user meant to go.
const INTERNAL_DISTROS: [&str; 2] = ["docker-desktop", "docker-desktop-data"];

/// The names of the WSL distributions installed here, in the order `wsl.exe`
/// reports them, minus the ones nobody opens a shell in.
///
/// Empty when WSL is not installed, when `wsl.exe` fails for any reason, or
/// when the machine genuinely has no distributions — the caller treats all
/// three the same way, by offering no distribution buttons.
pub fn list_distros() -> Vec<String> {
    let output = Command::new("wsl.exe")
        .args(["-l", "-q"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Ok(output) if output.status.success() => parse_distros(&output.stdout),
        // A machine without WSL is the common case here rather than an error
        // worth a warning: the failure is that `wsl.exe` is not on the path.
        Ok(output) => {
            log::debug!("wsl.exe -l -q exited with {}", output.status);
            Vec::new()
        }
        Err(error) => {
            log::debug!("wsl.exe could not be run: {error}");
            Vec::new()
        }
    }
}

/// Turns the raw stdout of `wsl.exe -l -q` into distribution names.
///
/// Split out from [`list_distros`] so the decoding can be tested without a
/// machine that has WSL on it.
fn parse_distros(stdout: &[u8]) -> Vec<String> {
    decode_utf16le(stdout)
        .lines()
        // `wsl.exe` writes CRLF, and pads short reads with NULs when the pipe
        // is drained mid-character; neither belongs in a distribution name,
        // and neither does the byte-order mark the first line may carry.
        .map(|line| {
            line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\0' || ch == '\u{feff}')
        })
        .filter(|line| !line.is_empty() && !INTERNAL_DISTROS.contains(line))
        .map(str::to_owned)
        .collect()
}

/// Decodes UTF-16LE, the encoding `wsl.exe` writes its listings in.
///
/// Not a choice this end makes: `wsl.exe` writes UTF-16 to its standard output
/// whether or not a console is attached, so the bytes are two per character
/// with a NUL in every other position, and reading them as UTF-8 would produce
/// a name per character. A trailing odd byte — a truncated pipe — is dropped
/// rather than guessed at, and an unpaired surrogate becomes the replacement
/// character, so no input can make this fail.
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a string the way `wsl.exe` writes one, for the tests below.
    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn a_listing_is_read_as_utf16_and_split_on_lines() {
        // Byte for byte what `wsl.exe -l -q` printed on the machine this was
        // written on: UTF-16LE, CRLF line endings, no byte-order mark.
        let stdout = utf16le("Ubuntu\r\ndocker-desktop\r\n");
        assert_eq!(parse_distros(&stdout), vec!["Ubuntu".to_owned()]);
    }

    #[test]
    fn a_leading_byte_order_mark_is_not_part_of_the_first_name() {
        let stdout = utf16le("\u{feff}Ubuntu\r\nDebian\r\n");
        assert_eq!(
            parse_distros(&stdout),
            vec!["Ubuntu".to_owned(), "Debian".to_owned()]
        );
    }

    #[test]
    fn padding_and_blank_lines_are_dropped() {
        let stdout = utf16le("Ubuntu\r\n\r\nDebian\0\0\r\n\r\n");
        assert_eq!(
            parse_distros(&stdout),
            vec!["Ubuntu".to_owned(), "Debian".to_owned()]
        );
    }

    #[test]
    fn a_name_outside_ascii_survives_the_round_trip() {
        let stdout = utf16le("Ubuntu\r\n우분투\r\n");
        assert_eq!(
            parse_distros(&stdout),
            vec!["Ubuntu".to_owned(), "우분투".to_owned()]
        );
    }

    #[test]
    fn a_machine_with_no_distributions_lists_none() {
        assert!(parse_distros(&[]).is_empty());
        assert!(parse_distros(&utf16le("\r\n")).is_empty());
        // Docker Desktop alone is not a distribution the user can work in.
        assert!(parse_distros(&utf16le("docker-desktop\r\ndocker-desktop-data\r\n")).is_empty());
    }

    #[test]
    fn a_truncated_last_character_is_dropped_rather_than_guessed() {
        let mut stdout = utf16le("Ubuntu\r\n");
        stdout.push(0x44);
        assert_eq!(parse_distros(&stdout), vec!["Ubuntu".to_owned()]);
    }
}
