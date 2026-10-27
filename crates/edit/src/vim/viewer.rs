use super::*;

pub fn handle_viewer_key(
    key: KeyEvent,
    mode: &mut VimMode,
    state: &mut VimWindowState,
) -> DocumentKeyResult {
    if matches!(state.sub, SubState::WaitingRegister) {
        state.name_register(key);
        return DocumentKeyResult::Consumed;
    }
    let result = viewer_key(key, mode, state);
    // A command keeps its register for the host, which takes it.
    if !matches!(result, DocumentKeyResult::Command(_)) {
        state.settle_register(false);
    }
    result
}

fn viewer_key(key: KeyEvent, mode: &mut VimMode, state: &mut VimWindowState) -> DocumentKeyResult {
    if *mode == VimMode::Insert {
        state.set_mode(mode, VimMode::Normal);
        return DocumentKeyResult::Consumed;
    }

    if key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
        )
    {
        return DocumentKeyResult::Passthrough;
    }

    let command = |state: &mut VimWindowState, cmd: Option<DocumentCommand>| {
        state.reset_counts();
        cmd.map_or(DocumentKeyResult::Consumed, DocumentKeyResult::Command)
    };
    match state.sub {
        SubState::WaitingOp(Op::Yank) => {
            if let KeyCode::Char(c @ ('[' | ']')) = key.code {
                state.sub = SubState::WaitingBracket(Some(Op::Yank), c == ']');
                return DocumentKeyResult::Consumed;
            }
            state.sub = SubState::Ready;
            let yank = |motion, state: &mut VimWindowState| {
                Some(DocumentCommand::YankMotion {
                    motion,
                    count: state.take_typed_count(),
                })
            };
            let cmd = match key.code {
                KeyCode::Char('y') => Some(DocumentCommand::YankLines(
                    state.effective_count() as crate::RowIndex
                )),
                KeyCode::Char(c @ ('i' | 'a')) => {
                    state.sub = SubState::WaitingTextObj(Op::Yank, c == 'i');
                    return DocumentKeyResult::Consumed;
                }
                KeyCode::Char('g') => {
                    state.sub = SubState::WaitingOpG(Op::Yank);
                    return DocumentKeyResult::Consumed;
                }
                KeyCode::Char(c) if find_kind(c).is_some() => {
                    state.sub = SubState::WaitingOpFind(Op::Yank, find_kind(c).unwrap());
                    return DocumentKeyResult::Consumed;
                }
                KeyCode::Char(c @ (';' | ',')) => Motion::repeat_find(state.last_find, c == ',')
                    .and_then(|motion| yank(motion, state)),
                code => Motion::from_key(code).and_then(|motion| yank(motion, state)),
            };
            return command(state, cmd);
        }
        SubState::WaitingOpG(Op::Yank) => {
            state.sub = SubState::Ready;
            let cmd = motion_after_g(key).map(|motion| DocumentCommand::YankMotion {
                motion,
                count: state.take_typed_count(),
            });
            return command(state, cmd);
        }
        SubState::WaitingOpFind(Op::Yank, kind) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(ch) => {
                    state.last_find = Some((kind, ch));
                    Some(DocumentCommand::YankMotion {
                        motion: Motion::Find {
                            kind,
                            ch,
                            repeat: false,
                        },
                        count: state.take_typed_count(),
                    })
                }
                _ => None,
            };
            return command(state, cmd);
        }
        SubState::WaitingFind(kind) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(ch) => {
                    state.last_find = Some((kind, ch));
                    Some(DocumentCommand::Move {
                        motion: Motion::Find {
                            kind,
                            ch,
                            repeat: false,
                        },
                        count: state.take_typed_count(),
                    })
                }
                _ => None,
            };
            return command(state, cmd);
        }
        SubState::WaitingTextObj(Op::Yank, inner) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(kind) => DocumentTextObject::new(inner, kind).map(|object| {
                    DocumentCommand::YankTextObject(object.counted(state.effective_count()))
                }),
                _ => None,
            };
            return command(state, cmd);
        }
        SubState::WaitingVisualTextObj(inner) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(kind) => DocumentTextObject::new(inner, kind)
                    .map(|object| DocumentCommand::TextObject(object.counted(state.take_count()))),
                _ => None,
            };
            return command(state, cmd);
        }
        SubState::WaitingBracket(op, forward) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(kind) => {
                    let motion = Motion::Block(BlockMotion { kind, forward });
                    let count = state.take_typed_count();
                    // The viewer's only operator is yank.
                    Some(match op {
                        None => DocumentCommand::Move { motion, count },
                        Some(_) => DocumentCommand::YankMotion { motion, count },
                    })
                }
                _ => None,
            };
            return command(state, cmd);
        }
        SubState::WaitingMark(key_char) => {
            state.reset_pending();
            let command = match (key_char, key.code) {
                ('m', KeyCode::Char(name @ 'a'..='z')) => DocumentCommand::SetMark(name),
                ('m', _) => return DocumentKeyResult::Consumed,
                (_, KeyCode::Char('\'' | '`')) => DocumentCommand::Jump(JumpMove::Previous),
                (_, KeyCode::Char(name @ 'a'..='z')) => DocumentCommand::Jump(JumpMove::Mark(name)),
                _ => return DocumentKeyResult::Consumed,
            };
            return DocumentKeyResult::Command(command);
        }
        SubState::WaitingG => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char('f') => Some(DocumentCommand::OpenAction),
                KeyCode::Char('v') => Some(DocumentCommand::ReselectVisual),
                KeyCode::Char(c @ ('j' | 'k')) => {
                    let rows = state.take_count() as isize;
                    Some(DocumentCommand::MoveRows(if c == 'j' {
                        rows
                    } else {
                        -rows
                    }))
                }
                _ => motion_after_g(key).map(|motion| DocumentCommand::Move {
                    motion,
                    count: state.take_typed_count(),
                }),
            };
            return command(state, cmd);
        }
        SubState::WaitingZ => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char('z') => Some(DocumentCommand::ScrollCursor(ScrollTo::Center)),
                KeyCode::Char('t') => Some(DocumentCommand::ScrollCursor(ScrollTo::Top)),
                KeyCode::Char('b') => Some(DocumentCommand::ScrollCursor(ScrollTo::Bottom)),
                KeyCode::Char('h') => Some(DocumentCommand::PanColumns(-1)),
                KeyCode::Char('l') => Some(DocumentCommand::PanColumns(1)),
                _ => None,
            };
            return command(state, cmd);
        }
        _ => {}
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('u') => DocumentKeyResult::Command(DocumentCommand::HalfPageRows(-1)),
            KeyCode::Char('d') => DocumentKeyResult::Command(DocumentCommand::HalfPageRows(1)),
            KeyCode::Char('b') => DocumentKeyResult::Command(DocumentCommand::PageRows(-1)),
            KeyCode::Char('f') => DocumentKeyResult::Command(DocumentCommand::PageRows(1)),
            KeyCode::Char('y') => DocumentKeyResult::Command(DocumentCommand::ScrollRows(-1)),
            KeyCode::Char('e') => DocumentKeyResult::Command(DocumentCommand::ScrollRows(1)),
            KeyCode::Char('o') => DocumentKeyResult::Command(DocumentCommand::Jump(
                JumpMove::Older(state.take_count()),
            )),
            KeyCode::Char('i') => DocumentKeyResult::Command(DocumentCommand::Jump(
                JumpMove::Newer(state.take_count()),
            )),
            _ => DocumentKeyResult::Passthrough,
        };
    }

    let rows = |state: &mut VimWindowState| state.take_count() as isize;
    match key.code {
        KeyCode::Char(c @ '1'..='9') => {
            let digit = c.to_digit(10).unwrap() as usize;
            let prev = state.count1.unwrap_or(0);
            state.count1 = Some(prev.saturating_mul(10).saturating_add(digit));
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('0') if state.count1.is_some() => {
            let prev = state.count1.unwrap_or(0);
            state.count1 = Some(prev.saturating_mul(10));
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('g') => {
            state.sub = SubState::WaitingG;
            DocumentKeyResult::Consumed
        }
        KeyCode::Char(c @ ('[' | ']')) => {
            state.sub = SubState::WaitingBracket(None, c == ']');
            DocumentKeyResult::Consumed
        }
        KeyCode::Char(key_char @ ('m' | '\'' | '`')) => {
            state.sub = SubState::WaitingMark(key_char);
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('"') => {
            state.sub = SubState::WaitingRegister;
            DocumentKeyResult::Consumed
        }
        KeyCode::Tab => {
            DocumentKeyResult::Command(DocumentCommand::Jump(JumpMove::Newer(state.take_count())))
        }
        KeyCode::Char(c) if find_kind(c).is_some() => {
            state.sub = SubState::WaitingFind(find_kind(c).unwrap());
            DocumentKeyResult::Consumed
        }
        KeyCode::Char(c @ (';' | ',')) => {
            let motion = Motion::repeat_find(state.last_find, c == ',');
            let count = state.take_typed_count();
            motion.map_or(DocumentKeyResult::Consumed, |motion| {
                DocumentKeyResult::Command(DocumentCommand::Move { motion, count })
            })
        }
        KeyCode::Char('j') | KeyCode::Down => {
            DocumentKeyResult::Command(DocumentCommand::MoveRows(rows(state)))
        }
        KeyCode::Char('k') | KeyCode::Up => {
            DocumentKeyResult::Command(DocumentCommand::MoveRows(-rows(state)))
        }
        KeyCode::Char('v') => {
            state.set_mode(mode, VimMode::Visual);
            DocumentKeyResult::Command(DocumentCommand::StartVisual)
        }
        KeyCode::Char('V') => {
            state.set_mode(mode, VimMode::VisualLine);
            DocumentKeyResult::Command(DocumentCommand::StartVisualLine)
        }
        KeyCode::Char('i') if matches!(*mode, VimMode::Visual | VimMode::VisualLine) => {
            state.sub = SubState::WaitingVisualTextObj(true);
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('a') if matches!(*mode, VimMode::Visual | VimMode::VisualLine) => {
            state.sub = SubState::WaitingVisualTextObj(false);
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('y') => {
            if matches!(*mode, VimMode::Visual | VimMode::VisualLine) {
                let linewise = matches!(*mode, VimMode::VisualLine);
                state.set_mode(mode, VimMode::Normal);
                DocumentKeyResult::Command(if linewise {
                    DocumentCommand::YankSelectionLinewise
                } else {
                    DocumentCommand::YankSelection
                })
            } else {
                state.sub = SubState::WaitingOp(Op::Yank);
                DocumentKeyResult::Consumed
            }
        }
        KeyCode::Esc => {
            state.reset_pending();
            state.set_mode(mode, VimMode::Normal);
            DocumentKeyResult::Command(DocumentCommand::ClearSelection)
        }
        KeyCode::Char('z') => {
            state.sub = SubState::WaitingZ;
            DocumentKeyResult::Consumed
        }
        code if Motion::from_key(code).is_some() => {
            let motion = Motion::from_key(code).unwrap();
            DocumentKeyResult::Command(DocumentCommand::Move {
                motion,
                count: state.take_typed_count(),
            })
        }
        _ if !state.is_idle() => {
            // An invalid counted motion cancels the sequence, not the viewer.
            state.reset_pending();
            DocumentKeyResult::Consumed
        }
        _ => DocumentKeyResult::Passthrough,
    }
}
