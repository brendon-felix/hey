/* -------------------------------------------------------------------------- */
/*                                 notify.rs                                  */
/* -------------------------------------------------------------------------- */

//! Tells the user, wherever they are, that an answer they waited long for
//! has ended or that a tool call waits for them: the terminal's bell, and a
//! desktop notification the terminal shows (OSC 9 or OSC 777). Sent by the
//! terminal following the conversation, so it reaches the user's own
//! machine with a remote host, and over SSH.

use std::io::Write;

/// Rings the bell and asks the terminal to show `body` as a notification.
pub fn notify(body: &str) {
    let mut stdout = std::io::stdout();
    _ = stdout.write_all(sequence(body, &Terminal::here()).as_bytes());
    _ = stdout.flush();
}

/// What tells this terminal apart, as far as notifications go.
struct Terminal {
    /// Takes OSC 777 rather than OSC 9, as VTE's, foot and urxvt do.
    osc_777: bool,
    /// Inside tmux, which passes a sequence on only wrapped.
    tmux: bool,
}

impl Terminal {
    fn here() -> Self {
        let var = |name| std::env::var(name).unwrap_or_default();
        let term = var("TERM");
        Self {
            osc_777: std::env::var_os("VTE_VERSION").is_some()
                || term.starts_with("foot")
                || term.starts_with("rxvt"),
            tmux: std::env::var_os("TMUX").is_some(),
        }
    }
}

fn sequence(body: &str, terminal: &Terminal) -> String {
    // A control character would end the sequence early.
    let body: String = body
        .chars()
        .filter(|c| !c.is_control() && *c != ';')
        .collect();
    let osc = match terminal.osc_777 {
        true => format!("\x1b]777;notify;hey;{body}\x1b\\"),
        false => format!("\x1b]9;hey: {body}\x1b\\"),
    };
    let osc = match terminal.tmux {
        true => format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")),
        false => osc,
    };
    format!("\x07{osc}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings_and_notifies_as_the_terminal_takes_it() {
        let plain = Terminal {
            osc_777: false,
            tmux: false,
        };
        assert_eq!(
            sequence("done;\n now", &plain),
            "\x07\x1b]9;hey: done now\x1b\\"
        );
        let vte = Terminal {
            osc_777: true,
            tmux: false,
        };
        assert_eq!(sequence("done", &vte), "\x07\x1b]777;notify;hey;done\x1b\\");
        let tmux = Terminal {
            osc_777: false,
            tmux: true,
        };
        assert_eq!(
            sequence("done", &tmux),
            "\x07\x1bPtmux;\x1b\x1b]9;hey: done\x1b\x1b\\\x1b\\"
        );
    }
}
