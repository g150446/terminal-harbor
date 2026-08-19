use crate::harbor_peer;
use mux::termwiztermtab::TermWizTerminal;
use std::time::{Duration, Instant};
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use termwiz::surface::{Change, CursorShape, Position};
use termwiz::terminal::Terminal;

pub fn show(
    mut term: TermWizTerminal,
    server_id: String,
    workspace_id: String,
    host_label: String,
) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    let _ = harbor_peer::activate_workspace(&server_id, &workspace_id);
    let mut input = String::new();
    let mut screen = String::new();
    let mut status = String::new();
    let mut last_fetch = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or_else(Instant::now);

    loop {
        if last_fetch.elapsed() >= Duration::from_secs(1) {
            match harbor_peer::fetch_screen(&server_id, &workspace_id, 80) {
                Ok(text) => {
                    screen = text;
                    status.clear();
                }
                Err(_) => status = "Could not refresh the remote screen".to_string(),
            }
            last_fetch = Instant::now();
        }
        render(&mut term, &host_label, &screen, &input, &status)?;
        match term.poll_input(Some(Duration::from_millis(200))) {
            Ok(Some(InputEvent::Key(KeyEvent { key, modifiers }))) => {
                match handle_key(
                    &server_id,
                    &workspace_id,
                    key,
                    modifiers,
                    &mut input,
                    &mut status,
                )? {
                    KeyResult::Exit => return Ok(()),
                    KeyResult::Continue => {
                        last_fetch = Instant::now()
                            .checked_sub(Duration::from_secs(2))
                            .unwrap_or_else(Instant::now);
                    }
                    KeyResult::Redraw => {}
                }
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => return Ok(()),
        }
    }
}

enum KeyResult {
    Continue,
    Redraw,
    Exit,
}

fn handle_key(
    server_id: &str,
    workspace_id: &str,
    key: KeyCode,
    modifiers: Modifiers,
    input: &mut String,
    status: &mut String,
) -> anyhow::Result<KeyResult> {
    let send_key = |key: &str, status: &mut String| {
        match harbor_peer::send_key(server_id, workspace_id, key) {
            Ok(()) => status.clear(),
            Err(_) => *status = "Could not send the key".to_string(),
        }
        KeyResult::Continue
    };
    if modifiers.contains(Modifiers::CTRL) && matches!(key, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(send_key("ctrl-c", status));
    }
    if matches!(key, KeyCode::Tab) && modifiers.contains(Modifiers::SHIFT) {
        return Ok(send_key("shift-tab", status));
    }
    if matches!(key, KeyCode::Tab) {
        return Ok(send_key("tab", status));
    }
    Ok(match key {
        KeyCode::Escape if input.is_empty() => KeyResult::Exit,
        KeyCode::Escape => {
            input.clear();
            KeyResult::Redraw
        }
        KeyCode::Enter => {
            let text = std::mem::take(input);
            match harbor_peer::send_instruction(server_id, workspace_id, &text, true) {
                Ok(()) => status.clear(),
                Err(_) => *status = "Could not send the instruction".to_string(),
            }
            KeyResult::Continue
        }
        KeyCode::Backspace | KeyCode::Delete => {
            input.pop();
            KeyResult::Redraw
        }
        KeyCode::UpArrow => send_key("up", status),
        KeyCode::DownArrow => send_key("down", status),
        KeyCode::Char(' ') if input.is_empty() => send_key("space", status),
        KeyCode::Char(ch) if modifiers == Modifiers::NONE || modifiers == Modifiers::SHIFT => {
            input.push(ch);
            KeyResult::Redraw
        }
        _ => KeyResult::Redraw,
    })
}

fn render(
    term: &mut TermWizTerminal,
    host_label: &str,
    screen: &str,
    input: &str,
    status: &str,
) -> anyhow::Result<()> {
    let size = term.get_screen_size()?;
    let cols = size.cols.max(20);
    let rows = size.rows.max(6);
    let mut changes = vec![
        Change::ClearScreen(Default::default()),
        Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(0),
        },
        Change::Text(format!("Remote: {host_label}\r\n")),
        Change::Text("Esc closes. Enter sends. Empty Space / Shift+Tab / arrows / Ctrl-C go to the remote pane.\r\n\r\n".into()),
    ];
    let header_rows = 4usize;
    let input_rows = 2usize;
    let body_rows = rows.saturating_sub(header_rows + input_rows).max(1);
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(body_rows);
    for line in &lines[start..] {
        let mut clipped = line.chars().take(cols).collect::<String>();
        if clipped.len() < line.len() {
            // already truncated by chars
        }
        clipped.push_str("\r\n");
        changes.push(Change::Text(clipped));
    }
    let input_y = rows.saturating_sub(2);
    changes.push(Change::CursorPosition {
        x: Position::Absolute(0),
        y: Position::Absolute(input_y),
    });
    let prompt = if status.is_empty() {
        format!("> {input}")
    } else {
        format!("! {status}")
    };
    changes.push(Change::Text(prompt.chars().take(cols).collect()));
    changes.push(Change::CursorPosition {
        x: Position::Absolute((2 + input.chars().count()).min(cols.saturating_sub(1))),
        y: Position::Absolute(input_y),
    });
    changes.push(Change::CursorShape(CursorShape::BlinkingBar));
    term.render(&changes)?;
    term.flush()?;
    Ok(())
}
