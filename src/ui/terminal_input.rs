use gtk4 as gtk;
use gtk4::gdk;
use gtk4::prelude::*;
use vte4::prelude::*;

use std::cell::RefCell;
use std::rc::Rc;

use crate::agent::types::AgentType;
use crate::ipc::client::IpcClient;
use crate::ipc::proto::ClientMessage;

fn paste_into_buffered_terminal(
    terminal: &vte4::Terminal,
    input_buffer: &Rc<RefCell<String>>,
    text: &str,
) {
    input_buffer.borrow_mut().push_str(text);
    terminal.feed(text.as_bytes());
}

pub fn attach(
    terminal: &vte4::Terminal,
    agent_id: u32,
    agent_type: AgentType,
    input_buffer: Rc<RefCell<String>>,
    ipc: Rc<IpcClient>,
) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    if agent_type == AgentType::ClaudeCode || agent_type == AgentType::Codex {
        let term = terminal.clone();
        controller.connect_key_pressed(move |_, key, _code, modifiers| {
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);

            if ctrl && shift {
                match key {
                    gdk::Key::C => {
                        if term.has_selection() {
                            term.copy_clipboard_format(vte4::Format::Text);
                            return gtk::glib::Propagation::Stop;
                        }
                    }
                    gdk::Key::V => {
                        if let Some(display) = gdk::Display::default() {
                            let clipboard = display.clipboard();
                            let term = term.clone();
                            let input_buffer = input_buffer.clone();
                            clipboard.read_text_async(
                                None::<&gtk::gio::Cancellable>,
                                move |result| {
                                    if let Ok(Some(text)) = result {
                                        paste_into_buffered_terminal(&term, &input_buffer, &text);
                                    }
                                },
                            );
                        }
                        return gtk::glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            // Escape → interrupt the agent's current turn
            if matches!(key, gdk::Key::Escape) {
                input_buffer.borrow_mut().clear();
                ipc.send(&ClientMessage::Interrupt { agent_id });
                return gtk::glib::Propagation::Stop;
            }

            // Ctrl+C → clear input buffer (does NOT interrupt the agent)
            if ctrl {
                if let Some(ch) = key.to_unicode() {
                    if (ch == 'c' || ch == 'C') && term.has_selection() {
                        term.copy_clipboard_format(vte4::Format::Text);
                        return gtk::glib::Propagation::Stop;
                    }
                    if ch == 'c' || ch == 'C' {
                        let buf = input_buffer.borrow();
                        if !buf.is_empty() {
                            drop(buf);
                            input_buffer.borrow_mut().clear();
                            term.feed(b"^C\r\n");
                        } else {
                            return gtk::glib::Propagation::Proceed;
                        }
                        return gtk::glib::Propagation::Stop;
                    }
                    if ch == 'v' || ch == 'V' {
                        if let Some(display) = gdk::Display::default() {
                            let clipboard = display.clipboard();
                            let term = term.clone();
                            let input_buffer = input_buffer.clone();
                            clipboard.read_text_async(
                                None::<&gtk::gio::Cancellable>,
                                move |result| {
                                    if let Ok(Some(text)) = result {
                                        paste_into_buffered_terminal(&term, &input_buffer, &text);
                                    }
                                },
                            );
                        }
                        return gtk::glib::Propagation::Stop;
                    }
                }
            }

            // Enter → send buffered text as prompt
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                let prompt = input_buffer.borrow().clone();
                input_buffer.borrow_mut().clear();
                if !prompt.is_empty() {
                    // Clear the locally echoed text, driver will render "> prompt"
                    term.feed(b"\r\x1b[2K");
                    ipc.send(&ClientMessage::SendPrompt {
                        agent_id,
                        prompt,
                    });
                }
                return gtk::glib::Propagation::Stop;
            }

            // Backspace → remove last char from buffer + erase from display
            if matches!(key, gdk::Key::BackSpace) {
                let mut buf = input_buffer.borrow_mut();
                if buf.pop().is_some() {
                    term.feed(b"\x08 \x08");
                }
                return gtk::glib::Propagation::Stop;
            }

            // Regular character → buffer + echo
            if !ctrl {
                if let Some(ch) = key.to_unicode() {
                    input_buffer.borrow_mut().push(ch);
                    let mut utf8_buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut utf8_buf);
                    term.feed(s.as_bytes());
                    return gtk::glib::Propagation::Stop;
                }
            }

            gtk::glib::Propagation::Proceed
        });
    } else {
        controller.connect_key_pressed(move |_, key, _code, modifiers| {
            if let Some(bytes) = key_to_bytes(key, modifiers) {
                ipc.send(&ClientMessage::Input { agent_id, bytes });
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }

    terminal.add_controller(controller);
}

fn key_to_bytes(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<Vec<u8>> {
    let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);

    match key {
        gdk::Key::Return | gdk::Key::KP_Enter => return Some(b"\r".to_vec()),
        gdk::Key::BackSpace => return Some(vec![0x7f]),
        gdk::Key::Tab => return Some(b"\t".to_vec()),
        gdk::Key::Escape => return Some(vec![0x1b]),
        gdk::Key::Up => return Some(b"\x1b[A".to_vec()),
        gdk::Key::Down => return Some(b"\x1b[B".to_vec()),
        gdk::Key::Right => return Some(b"\x1b[C".to_vec()),
        gdk::Key::Left => return Some(b"\x1b[D".to_vec()),
        gdk::Key::Home => return Some(b"\x1b[H".to_vec()),
        gdk::Key::End => return Some(b"\x1b[F".to_vec()),
        gdk::Key::Page_Up => return Some(b"\x1b[5~".to_vec()),
        gdk::Key::Page_Down => return Some(b"\x1b[6~".to_vec()),
        gdk::Key::Delete => return Some(b"\x1b[3~".to_vec()),
        gdk::Key::Insert => return Some(b"\x1b[2~".to_vec()),
        _ => {}
    }

    if let Some(ch) = key.to_unicode() {
        if ctrl {
            let upper = ch.to_ascii_uppercase() as u8;
            let ctrl_code = upper & 0x1f;
            return Some(vec![ctrl_code]);
        }
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        return Some(s.as_bytes().to_vec());
    }

    None
}
