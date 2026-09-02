//! Interactive model questions shared by the agent worker and TUI thread.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub(crate) const TOOL_NAME: &str = "request_user_input";
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const FOLLOW_UP_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuestionOption {
    pub(crate) label: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserQuestion {
    pub(crate) id: String,
    pub(crate) question: String,
    pub(crate) options: Vec<QuestionOption>,
    pub(crate) recommended: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct QuestionSnapshot {
    pub(crate) request_id: u64,
    pub(crate) question_index: usize,
    pub(crate) question_count: usize,
    pub(crate) question: UserQuestion,
    pub(crate) remaining: Option<Duration>,
}

#[derive(Debug, Clone)]
struct Answer {
    id: String,
    option_index: Option<usize>,
    label: String,
    answer: String,
    source: &'static str,
}

struct Pending {
    id: u64,
    questions: Vec<UserQuestion>,
    current: usize,
    answers: Vec<Answer>,
    afk_timer: AfkTimer,
    timeout: Duration,
    follow_up_grace: Duration,
}

enum AfkTimer {
    Armed { show_at: Instant, deadline: Instant },
    Disabled,
}

impl AfkTimer {
    fn armed(timeout: Duration, grace: Duration) -> Self {
        let show_at = Instant::now() + grace;
        Self::Armed {
            show_at,
            deadline: show_at + timeout,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Armed { deadline, .. } => Some(*deadline),
            Self::Disabled => None,
        }
    }

    fn visible_remaining(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Armed { show_at, deadline } if now >= *show_at => {
                Some(deadline.saturating_duration_since(now))
            }
            Self::Armed { .. } | Self::Disabled => None,
        }
    }
}

#[derive(Default)]
struct State {
    enabled: bool,
    next_id: u64,
    timed_out_this_turn: bool,
    pending: Option<Pending>,
    completed: Option<(u64, String)>,
    canceled: Option<u64>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// Synchronous bridge used by the model tool and the responsive TUI loop.
#[derive(Clone, Default)]
pub(crate) struct QuestionBroker {
    shared: Arc<Shared>,
}

impl QuestionBroker {
    pub(crate) fn enable(&self) {
        self.lock().enabled = true;
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.lock().enabled
    }

    pub(crate) fn begin_turn(&self) {
        let mut state = self.lock();
        state.timed_out_this_turn = false;
        state.completed = None;
        state.canceled = None;
    }

    pub(crate) fn ask(&self, questions: Vec<UserQuestion>) -> Result<String, String> {
        self.ask_with_timing(questions, DEFAULT_TIMEOUT, FOLLOW_UP_GRACE)
    }

    #[cfg(test)]
    fn ask_with_timeout(
        &self,
        questions: Vec<UserQuestion>,
        timeout: Duration,
    ) -> Result<String, String> {
        self.ask_with_timing(questions, timeout, Duration::ZERO)
    }

    fn ask_with_timing(
        &self,
        questions: Vec<UserQuestion>,
        timeout: Duration,
        follow_up_grace: Duration,
    ) -> Result<String, String> {
        let mut state = self.lock();
        if !state.enabled {
            return Err("interactive user questions are unavailable".into());
        }
        if state.timed_out_this_turn {
            return Err(
                "the user timed out earlier in this turn; do not ask another question".into(),
            );
        }
        if state.pending.is_some() {
            return Err("another user question is already pending".into());
        }
        state.next_id = state.next_id.saturating_add(1);
        let request_id = state.next_id;
        state.pending = Some(Pending {
            id: request_id,
            questions,
            current: 0,
            answers: Vec::new(),
            afk_timer: AfkTimer::armed(timeout, Duration::ZERO),
            timeout,
            follow_up_grace,
        });
        self.shared.changed.notify_all();

        loop {
            if state.canceled == Some(request_id) || crate::cancellation::interrupted() {
                state.pending = None;
                state.canceled = None;
                self.shared.changed.notify_all();
                return Err("[interrupted by user]".into());
            }
            if let Some((completed_id, result)) = state.completed.take() {
                if completed_id == request_id {
                    return Ok(result);
                }
                state.completed = Some((completed_id, result));
            }
            let now = Instant::now();
            let deadline = state
                .pending
                .as_ref()
                .filter(|pending| pending.id == request_id)
                .ok_or_else(|| "interactive user question ended without a result".to_string())?
                .afk_timer
                .deadline();
            if deadline.is_some_and(|deadline| now >= deadline) {
                let pending = state
                    .pending
                    .take()
                    .ok_or_else(|| "interactive user question disappeared".to_string())?;
                let result = timeout_result(pending);
                state.timed_out_this_turn = true;
                self.shared.changed.notify_all();
                return Ok(result);
            }
            let wait = deadline.map_or(Duration::from_millis(100), |deadline| {
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(100))
            });
            state = match self.shared.changed.wait_timeout(state, wait) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    pub(crate) fn snapshot(&self) -> Option<QuestionSnapshot> {
        let state = self.lock();
        let pending = state.pending.as_ref()?;
        let question = pending.questions.get(pending.current)?.clone();
        Some(QuestionSnapshot {
            request_id: pending.id,
            question_index: pending.current,
            question_count: pending.questions.len(),
            question,
            remaining: pending.afk_timer.visible_remaining(Instant::now()),
        })
    }

    pub(crate) fn mark_present(&self, request_id: u64) -> Result<(), String> {
        let mut state = self.lock();
        let Some(pending) = state.pending.as_mut() else {
            return Err("there is no pending user question".into());
        };
        if pending.id != request_id {
            return Err("the user question is no longer active".into());
        }
        pending.afk_timer = AfkTimer::Disabled;
        self.shared.changed.notify_all();
        Ok(())
    }

    pub(crate) fn answer(&self, request_id: u64, option_index: usize) -> Result<(), String> {
        let mut state = self.lock();
        let Some(pending) = state.pending.as_mut() else {
            return Err("there is no pending user question".into());
        };
        if pending.id != request_id {
            return Err("the user question is no longer active".into());
        }
        let question = pending
            .questions
            .get(pending.current)
            .ok_or_else(|| "the current user question is invalid".to_string())?;
        let option = question
            .options
            .get(option_index)
            .ok_or_else(|| "the selected option does not exist".to_string())?;
        pending.answers.push(Answer {
            id: question.id.clone(),
            option_index: Some(option_index),
            label: option.label.clone(),
            answer: option.label.clone(),
            source: "user",
        });
        self.finish_answer(state, request_id)
    }

    pub(crate) fn answer_custom(&self, request_id: u64, answer: String) -> Result<(), String> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err("the custom answer cannot be empty".into());
        }
        let mut state = self.lock();
        let Some(pending) = state.pending.as_mut() else {
            return Err("there is no pending user question".into());
        };
        if pending.id != request_id {
            return Err("the user question is no longer active".into());
        }
        let question = pending
            .questions
            .get(pending.current)
            .ok_or_else(|| "the current user question is invalid".to_string())?;
        pending.answers.push(Answer {
            id: question.id.clone(),
            option_index: None,
            label: "Other".into(),
            answer: answer.to_string(),
            source: "user",
        });
        self.finish_answer(state, request_id)
    }

    fn finish_answer(
        &self,
        mut state: MutexGuard<'_, State>,
        request_id: u64,
    ) -> Result<(), String> {
        let pending = state
            .pending
            .as_mut()
            .ok_or_else(|| "there is no pending user question".to_string())?;
        pending.current = pending.current.saturating_add(1);
        if pending.current < pending.questions.len() {
            pending.afk_timer = AfkTimer::armed(pending.timeout, pending.follow_up_grace);
            self.shared.changed.notify_all();
            return Ok(());
        }
        let pending = state
            .pending
            .take()
            .ok_or_else(|| "the completed user question disappeared".to_string())?;
        state.completed = Some((request_id, render_result(pending.answers, false)));
        self.shared.changed.notify_all();
        Ok(())
    }

    pub(crate) fn cancel_pending(&self) {
        let mut state = self.lock();
        if let Some(pending) = state.pending.take() {
            state.canceled = Some(pending.id);
            self.shared.changed.notify_all();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) fn parse_questions(args: &Value) -> Result<Vec<UserQuestion>, String> {
    let values = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| "'questions' must be an array".to_string())?;
    if !(1..=3).contains(&values.len()) {
        return Err("'questions' must contain between 1 and 3 questions".into());
    }
    let mut questions = Vec::with_capacity(values.len());
    for value in values {
        let id = required_string(value, "id")?;
        if questions
            .iter()
            .any(|question: &UserQuestion| question.id == id)
        {
            return Err(format!("duplicate question id '{id}'"));
        }
        let question = required_string(value, "question")?;
        let options = value
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("question '{id}' options must be an array"))?;
        if !(2..=3).contains(&options.len()) {
            return Err(format!("question '{id}' must contain 2 or 3 options"));
        }
        let options = options
            .iter()
            .map(|option| {
                Ok(QuestionOption {
                    label: option_label(option)?,
                    description: required_string(option, "description")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let recommended = value
            .get("recommended")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| format!("question '{id}' recommended must be an option index"))?;
        if recommended >= options.len() {
            return Err(format!("question '{id}' recommended option does not exist"));
        }
        questions.push(UserQuestion {
            id,
            question,
            options,
            recommended,
        });
    }
    Ok(questions)
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("'{key}' must be a non-empty string"))
}

fn option_label(value: &Value) -> Result<String, String> {
    const RECOMMENDED_MARKER: &str = "(Recommended)";

    let mut label = required_string(value, "label")?;
    while let Some(start) = label.len().checked_sub(RECOMMENDED_MARKER.len()) {
        let Some(suffix) = label.get(start..) else {
            break;
        };
        if !suffix.eq_ignore_ascii_case(RECOMMENDED_MARKER) {
            break;
        }
        let prefix_len = label
            .get(..start)
            .map(str::trim_end)
            .map(str::len)
            .unwrap_or(0);
        label.truncate(prefix_len);
    }
    if label.is_empty() {
        return Err("'label' must contain text besides '(Recommended)'".into());
    }
    Ok(label)
}

fn timeout_result(mut pending: Pending) -> String {
    for question in &pending.questions[pending.current..] {
        let option = &question.options[question.recommended];
        pending.answers.push(Answer {
            id: question.id.clone(),
            option_index: Some(question.recommended),
            label: option.label.clone(),
            answer: option.label.clone(),
            source: "timeout",
        });
    }
    render_result(pending.answers, true)
}

fn render_result(answers: Vec<Answer>, timed_out: bool) -> String {
    let answers = answers
        .into_iter()
        .map(|answer| {
            json!({
                "id": answer.id,
                "option_index": answer.option_index,
                "label": answer.label,
                "answer": answer.answer,
                "source": answer.source,
            })
        })
        .collect::<Vec<_>>();
    let mut result = json!({
        "answers": answers,
        "timed_out": timed_out,
    });
    if timed_out {
        result["message"] = Value::String(
            "The user did not respond. Do not call request_user_input again in this turn.".into(),
        );
    }
    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{\"timed_out\":true}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> Vec<UserQuestion> {
        (0..3)
            .map(|index| UserQuestion {
                id: format!("q{index}"),
                question: format!("Question {index}?"),
                options: vec![
                    QuestionOption {
                        label: "First".into(),
                        description: "first choice".into(),
                    },
                    QuestionOption {
                        label: "Second".into(),
                        description: "second choice".into(),
                    },
                ],
                recommended: 1,
            })
            .collect()
    }

    #[test]
    fn parser_requires_options_and_a_valid_recommendation() {
        let valid = json!({
            "questions": [{
                "id": "scope",
                "question": "Which scope?",
                "options": [
                    {"label": "Small", "description": "focused"},
                    {"label": "Large", "description": "broad"}
                ],
                "recommended": 0
            }]
        });
        assert_eq!(parse_questions(&valid).expect("valid questions").len(), 1);

        let mut invalid = valid;
        invalid["questions"][0]["recommended"] = json!(9);
        assert!(parse_questions(&invalid).is_err());
    }

    #[test]
    fn parser_removes_recommended_markers_from_option_labels() {
        let value = json!({
            "questions": [{
                "id": "scope",
                "question": "Which scope?",
                "options": [
                    {
                        "label": "Product polish (Recommended) (recommended)",
                        "description": "focused"
                    },
                    {"label": "Reliability", "description": "broad"}
                ],
                "recommended": 0
            }]
        });

        let questions = parse_questions(&value).expect("question should normalize");
        assert_eq!(questions[0].options[0].label, "Product polish");
    }

    #[test]
    fn timeout_defaults_current_and_remaining_questions() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let result = broker
            .ask_with_timeout(questions(), Duration::from_millis(1))
            .expect("timeout should return recommendations");
        let value: Value = serde_json::from_str(&result).expect("result json");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["answers"].as_array().map(Vec::len), Some(3));
        assert_eq!(value["answers"][0]["option_index"], 1);
        assert!(
            broker
                .ask_with_timeout(questions(), Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn moving_the_selection_disables_the_current_question_timeout() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let handle = std::thread::spawn(move || {
            asker.ask_with_timeout(questions(), Duration::from_millis(20))
        });
        while broker.snapshot().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }

        let first = broker.snapshot().expect("first question");
        assert!(first.remaining.is_some());
        broker
            .mark_present(first.request_id)
            .expect("selection movement should mark the user present");
        std::thread::sleep(Duration::from_millis(50));
        let still_pending = broker
            .snapshot()
            .expect("a present user should not be timed out");
        assert!(still_pending.remaining.is_none());

        broker.cancel_pending();
        assert!(handle.join().expect("asker thread").is_err());
    }

    #[test]
    fn follow_up_question_hides_its_countdown_during_the_grace_period() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let handle = std::thread::spawn(move || {
            asker.ask_with_timing(
                questions(),
                Duration::from_millis(80),
                Duration::from_millis(80),
            )
        });
        while broker.snapshot().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }

        let first = broker.snapshot().expect("first question");
        assert!(first.remaining.is_some());
        broker
            .answer(first.request_id, first.question.recommended)
            .expect("first answer");
        let second = broker.snapshot().expect("second question");
        assert!(second.remaining.is_none());
        std::thread::sleep(Duration::from_millis(95));
        let counting_down = broker.snapshot().expect("second question countdown");
        assert!(counting_down.remaining.is_some());

        broker.cancel_pending();
        assert!(handle.join().expect("asker thread").is_err());
    }

    #[test]
    fn answering_one_question_resets_the_next_deadline() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let handle = std::thread::spawn(move || {
            asker.ask_with_timeout(questions(), Duration::from_millis(100))
        });
        std::thread::sleep(Duration::from_millis(60));
        let first = broker.snapshot().expect("first question");
        broker
            .answer(first.request_id, first.question.recommended)
            .expect("first answer");
        std::thread::sleep(Duration::from_millis(60));
        let second = broker
            .snapshot()
            .expect("the second question should have a fresh deadline");
        assert_eq!(second.question_index, 1);
        broker.answer(second.request_id, 0).expect("second answer");
        let third = broker.snapshot().expect("third question");
        broker.answer(third.request_id, 0).expect("third answer");
        let result = handle
            .join()
            .expect("asker thread")
            .expect("completed answers");
        let value: Value = serde_json::from_str(&result).expect("result json");
        assert_eq!(value["timed_out"], false);
        assert_eq!(value["answers"][0]["source"], "user");
    }

    #[test]
    fn custom_answer_has_text_and_no_model_option_index() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let handle =
            std::thread::spawn(move || asker.ask_with_timeout(questions(), Duration::from_secs(2)));
        while broker.snapshot().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }

        let first = broker.snapshot().expect("first question");
        broker
            .answer_custom(
                first.request_id,
                "Use the existing behavior, but simplify it".into(),
            )
            .expect("custom answer");
        for _ in 0..2 {
            let question = broker.snapshot().expect("remaining question");
            broker
                .answer(question.request_id, question.question.recommended)
                .expect("recommended answer");
        }

        let result = handle
            .join()
            .expect("asker thread")
            .expect("completed answers");
        let value: Value = serde_json::from_str(&result).expect("result json");
        assert!(value["answers"][0]["option_index"].is_null());
        assert_eq!(value["answers"][0]["label"], "Other");
        assert_eq!(
            value["answers"][0]["answer"],
            "Use the existing behavior, but simplify it"
        );
    }

    #[test]
    fn cancellation_wakes_a_blocked_question_immediately() {
        let broker = QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            asker.ask_with_timeout(questions(), Duration::from_secs(10))
        });
        while broker.snapshot().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        broker.cancel_pending();
        assert!(handle.join().expect("asker thread").is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
