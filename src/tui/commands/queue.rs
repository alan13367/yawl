//! `/unqueue` and the queue picker: editing, reordering, and removing
//! messages waiting for the active response.

use super::super::picker::{Picker, PickerAction, PickerItem};
use super::super::state::ViewState;

fn queue_picker(state: &ViewState, selected: usize) -> Option<Picker> {
    if state.queued_inputs.is_empty() {
        return None;
    }
    let mut items = state
        .queued_inputs
        .iter()
        .enumerate()
        .map(|(index, input)| PickerItem {
            label: format!("Queued {}", index + 1),
            description: input.text.replace('\n', " "),
            action: PickerAction::SendQueued(index),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Clear all queued messages".into(),
        description: format!("Remove all {} pending", state.queued_inputs.len()),
        action: PickerAction::ClearQueued,
    });
    Some(Picker {
        title: "Queued messages".into(),
        hint: "↑/↓ select  K/J reorder  e edit  d delete  Enter send now".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
        parent: None,
    })
}

pub(in crate::tui) fn open_queue_picker(state: &mut ViewState) {
    state.picker = queue_picker(state, 0);
    if state.picker.is_none() {
        state.activity = "no queued messages".into();
    }
}

fn remove_queued(state: &mut ViewState, index: usize) -> bool {
    if state.queued_inputs.remove(index).is_some() {
        if state.queued_inputs.is_empty() {
            state.queue_paused = false;
        }
        state.activity = format!("removed queued message {}", index + 1);
        state.scroll_offset = 0;
        true
    } else {
        state.activity = format!("queued message {} does not exist", index + 1);
        false
    }
}

fn clear_queued(state: &mut ViewState) {
    let count = state.queued_inputs.len();
    state.queued_inputs.clear();
    state.queue_paused = false;
    state.activity = match count {
        0 => "no queued messages".into(),
        1 => "removed 1 queued message".into(),
        _ => format!("removed {count} queued messages"),
    };
    state.scroll_offset = 0;
}

fn move_queued(state: &mut ViewState, index: usize, direction: isize) -> usize {
    if state.queued_inputs.is_empty() || index >= state.queued_inputs.len() {
        return index;
    }
    let target = if direction < 0 {
        index.saturating_sub(direction.unsigned_abs())
    } else {
        index
            .saturating_add(direction as usize)
            .min(state.queued_inputs.len() - 1)
    };
    if target != index
        && let Some(input) = state.queued_inputs.remove(index)
    {
        state.queued_inputs.insert(target, input);
        state.activity = format!("moved queued message {} to {}", index + 1, target + 1);
    }
    target
}

pub(in crate::tui) fn promote_queued(state: &mut ViewState, index: usize) -> bool {
    if index >= state.queued_inputs.len() {
        return false;
    }
    state.queue_paused = false;
    let _ = move_queued(state, index, -(index as isize));
    state.activity = "stopping the active turn to send queued message".into();
    true
}

pub(in crate::tui) fn unqueue(argument: &str, state: &mut ViewState) {
    match argument {
        "" => open_queue_picker(state),
        "all" => clear_queued(state),
        number => match number
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
        {
            Some(index) => {
                let _ = remove_queued(state, index);
            }
            None => state.activity = "usage: /unqueue [NUMBER|all]".into(),
        },
    }
}

pub(in crate::tui) fn handle_queue_picker_action(
    state: &mut ViewState,
    action: PickerAction,
) -> Option<PickerAction> {
    match action {
        PickerAction::ApplyQueued { index, input } => {
            if let Some(queued) = state.queued_inputs.get_mut(index) {
                *queued = input;
                state.activity = format!("updated queued message {}", index + 1);
            }
            state.picker = queue_picker(state, index);
            None
        }
        PickerAction::MoveQueued { index, direction } => {
            let selected = move_queued(state, index, direction);
            state.picker = queue_picker(state, selected);
            None
        }
        PickerAction::RemoveQueued(index) => {
            if remove_queued(state, index) {
                state.picker = queue_picker(state, index);
            }
            None
        }
        PickerAction::ClearQueued => {
            clear_queued(state);
            None
        }
        action => Some(action),
    }
}
