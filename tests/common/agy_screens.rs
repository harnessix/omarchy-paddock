//! The screens of agy 1.4, recorded at 49 columns on a real login, and a
//! fake terminal interface that draws them and reads keys. The unit tests
//! of training.rs and the end-to-end test of the training gate share them.
//! The screens after the first key are the recorded one with the box or
//! the focus changed.
#![allow(dead_code)]

/// agy 1.4 after the theme choice: "Terms of Service & Data Use".
pub const AFTER_THEME: &str = "
    ▄▀▀▄
   ▀▀▀▀▀▀
  ▀▀▀▀▀▀▀▀
 ▄▀▀    ▀▀▄
▄▀▀      ▀▀▄

Terms of Service & Data Use

AI coding agents are known to have certain
security risks, including autonomous code
execution, data exfiltration, prompt injection
and supply chain risks. Ensure that you monitor
and verify all actions taken by the agent.

-------------------------------------------------

  > [x] Yes, I agree to help improve Antigravity
      CLI by allowing Google to collect and use
      my Interactions data, subject to the
      Google Antigravity CLI Terms of Service
      and Google Privacy Policy. I understand I
      can choose to opt out later whenever I
      want via my settings.

      Links:
      - Terms of Service:
        https://antigravity.google/terms
      - Privacy Policy:
        https://policies.google.com/privacy

    [Previous]      [Done]


  ↑/↓ Navigate · enter Toggle
                 auto · Gemini 3.8 Flash · medium
";

/// agy 1.4 after the login: the theme choice.
pub const AFTER_LOGIN: &str = "
    ▄▀▀▄
   ▀▀▀▀▀▀
  ▀▀▀▀▀▀▀▀
 ▄▀▀    ▀▀▄
▄▀▀      ▀▀▄

Welcome to Antigravity CLI!

Choose your color scheme:

  > terminal
    light
    solarized light
    colorblind-friendly light
    dark
    solarized dark
    colorblind-friendly dark
    tokyo night
  (widen the terminal to see a preview)

    [Next]

  ↑/↓ Navigate · enter Confirm
                 auto · Gemini 3.8 Flash · medium
";

/// agy 1.4 before the login.
pub const LOGIN: &str = "
     ▄▀▀▄
    ▀▀▀▀▀▀
   ▀▀▀▀▀▀▀▀
  ▄▀▀    ▀▀▄
 ▄▀▀      ▀▀▄

 Welcome to the Antigravity CLI. You are currently not signed in.

 Select login method:
 > 1. Google OAuth
   2. Use a Google Cloud project

   ↑/↓ Navigate · enter Select
";

/// agy 1.4 after [Done]: the folder trust question.
pub const AFTER_DONE: &str = "Accessing workspace:

/home/demo/proj

Do you trust the contents of this project?

Antigravity CLI requires permission to read,
edit, and execute files here.

> Yes, I trust this folder
  No, exit

  ↑/↓ Navigate · enter Confirm
                 auto · Gemini 3.8 Flash · medium
";

const BOX: &str = "  > [x] Yes, I agree to help improve Antigravity";
const BUTTONS: &str = "    [Previous]      [Done]";

/// The dialog with the box checked or not, and the focus on `box`,
/// `prev` or `done`.
pub fn dialog(checked: bool, focus: &str) -> String {
    let mark = if checked { "[x]" } else { "[ ]" };
    let boxrow = match focus {
        "box" => format!("  > {mark} Yes, I agree to help improve Antigravity"),
        _ => format!("    {mark} Yes, I agree to help improve Antigravity"),
    };
    let buttons = match focus {
        "prev" => "  > [Previous]      [Done]",
        "done" => "    [Previous]    > [Done]",
        _ => BUTTONS,
    };
    let mut s = AFTER_THEME.replace(BOX, &boxrow).replace(BUTTONS, buttons);
    if focus != "box" {
        s = s.replace(
            "  ↑/↓ Navigate · enter Toggle\n",
            "  ↑/↓ Navigate · enter Confirm\n",
        );
    }
    s
}

/// A fake agy dialog: `sh -c FAKE_TUI sh DIR FIRST`. It draws the screen
/// files that `write_screens` put into DIR, reads keys from its terminal,
/// and moves the focus from the box down to FIRST (`done` or `prev`).
/// [Done] writes `box=x` (checked) or `box=o` (unchecked) into DIR/result.
/// It also writes its terminal size into DIR/size.
pub const FAKE_TUI: &str = r#"d="$1"; first="$2"
stty size > "$d/size"
stty raw -echo
box=x; focus=box
draw() { printf '\033[H\033[2J'; cat "$d/$box-$focus"; }
draw
while :; do
  b=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
  [ -z "$b" ] && exit 1
  if [ "$b" = 1b ]; then b="1b$(dd bs=1 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')"; fi
  case "$focus:$b" in
    box:0d) if [ "$box" = x ]; then box=o; else box=x; fi ;;
    box:1b5b42) focus="$first" ;;
    prev:1b5b43) focus=done ;;
    done:1b5b44) focus=prev ;;
    prev:1b5b41|done:1b5b41) focus=box ;;
    done:0d) echo "box=$box" > "$d/result"; printf '\033[H\033[2J'; cat "$d/trust"; sleep 2; exit 0 ;;
    prev:0d) echo previous > "$d/result"; exit 3 ;;
    *) echo "key=$b" >> "$d/other" ;;
  esac
  draw
done
"#;

/// The screen files of FAKE_TUI: `x-box`, `o-done` and so on, and `trust`.
pub fn write_screens(dir: &std::path::Path) {
    for (checked, c) in [(true, "x"), (false, "o")] {
        for focus in ["box", "prev", "done"] {
            let text = dialog(checked, focus).replace('\n', "\r\n");
            std::fs::write(dir.join(format!("{c}-{focus}")), text).unwrap();
        }
    }
    std::fs::write(dir.join("trust"), AFTER_DONE.replace('\n', "\r\n")).unwrap();
}
