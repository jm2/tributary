//! Serialized library-root trust prompts.
//!
//! The engine owns all filesystem evidence and performs the final guarded
//! confirmation. This module only queues prompts on the GTK thread, presents
//! one dialog at a time, and forwards an affirmative response without ever
//! treating dismissal or an unknown response as consent.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use crate::local::engine::{LibraryCommand, RootTrustOutcome, RootTrustReason, RootTrustRequest};

use super::library_commands::LibraryCommandAdmission;

const CONFIRM_RESPONSE: &str = "confirm";
const DEFER_RESPONSE: &str = "defer";

#[derive(Debug)]
struct QueuedPrompt<T> {
    id: String,
    payload: T,
}

/// A GTK-independent FIFO with lifetime duplicate suppression.
///
/// IDs remain in `seen` after their dialog closes. Repeated reconciliation
/// events therefore cannot nag the user again during the same process, while
/// a materially different engine request (with a new ID) can still be shown.
#[derive(Debug)]
struct PromptQueue<T> {
    pending: VecDeque<QueuedPrompt<T>>,
    active_id: Option<String>,
    active_stage: Option<PromptStage>,
    seen: HashSet<String>,
}

impl<T> PromptQueue<T> {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            active_id: None,
            active_stage: None,
            seen: HashSet::new(),
        }
    }

    fn enqueue(&mut self, id: String, payload: T) -> bool {
        if !self.seen.insert(id.clone()) {
            return false;
        }
        self.pending.push_back(QueuedPrompt { id, payload });
        true
    }

    fn take_next(&mut self) -> Option<QueuedPrompt<T>> {
        if self.active_id.is_some() {
            return None;
        }
        let prompt = self.pending.pop_front()?;
        self.active_id = Some(prompt.id.clone());
        self.active_stage = Some(PromptStage::Initial);
        Some(prompt)
    }

    fn advance(&mut self, id: &str, from: PromptStage, to: PromptStage) -> bool {
        if self.active_id.as_deref() != Some(id) || self.active_stage != Some(from) {
            return false;
        }
        self.active_stage = Some(to);
        true
    }

    fn finish(&mut self, id: &str, stage: PromptStage) -> bool {
        if self.active_id.as_deref() != Some(id) || self.active_stage != Some(stage) {
            return false;
        }
        self.active_id = None;
        self.active_stage = None;
        true
    }

    /// Permit a later engine event to requeue a request whose command could
    /// not be delivered. Deliberate dismissal and successful delivery retain
    /// lifetime suppression.
    fn allow_retry(&mut self, id: &str) {
        self.seen.remove(id);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptStage {
    Initial,
    EmptyFinal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseAction {
    Defer,
    ContinueToEmptyFinal,
    Confirm,
}

struct ControllerInner {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    command_admission: LibraryCommandAdmission,
    queue: RefCell<PromptQueue<RootTrustRequest>>,
    finished_ids: RefCell<HashSet<String>>,
}

/// Owns the root-trust prompt queue on the GTK main thread.
#[derive(Clone)]
pub struct RootTrustPromptController {
    inner: Rc<ControllerInner>,
}

impl RootTrustPromptController {
    pub(super) fn new(
        window: &adw::ApplicationWindow,
        toast_overlay: &adw::ToastOverlay,
        command_admission: LibraryCommandAdmission,
    ) -> Self {
        Self {
            inner: Rc::new(ControllerInner {
                window: window.clone(),
                toast_overlay: toast_overlay.clone(),
                command_admission,
                queue: RefCell::new(PromptQueue::new()),
                finished_ids: RefCell::new(HashSet::new()),
            }),
        }
    }

    /// Queue all newly reported roots and present the first one, if idle.
    pub fn enqueue(&self, requests: Vec<RootTrustRequest>) {
        if !self.inner.command_admission.is_open() {
            return;
        }
        let mut added = false;
        {
            let mut queue = self.inner.queue.borrow_mut();
            for request in requests {
                let id = request.request_id().to_string();
                added |= queue.enqueue(id, request);
            }
        }
        if added {
            self.present_next();
        }
    }

    /// Surface the engine's guarded confirmation result once.
    pub fn handle_finished<I, P>(
        &self,
        request_id: I,
        path: P,
        reason: RootTrustReason,
        outcome: RootTrustOutcome,
    ) where
        I: ToString,
        P: AsRef<Path>,
    {
        let request_id = request_id.to_string();
        if !self
            .inner
            .finished_ids
            .borrow_mut()
            .insert(request_id.clone())
        {
            return;
        }

        let path = path.as_ref();
        tracing::info!(root = %path.display(), ?outcome, "Library root trust request finished");
        let message = match outcome {
            RootTrustOutcome::Active => match reason {
                RootTrustReason::LegacyEnrollment => {
                    rust_i18n::t!("library_root_trust.trusted_toast")
                }
                RootTrustReason::Replacement => {
                    rust_i18n::t!("library_root_trust.replacement_trusted_toast")
                }
                RootTrustReason::EmptyRoot => {
                    rust_i18n::t!("library_root_trust.empty_trusted_toast")
                }
            },
            RootTrustOutcome::TrustedButUnavailable => {
                rust_i18n::t!("library_root_trust.unavailable_toast")
            }
            RootTrustOutcome::Stale => rust_i18n::t!("library_root_trust.stale_toast"),
            RootTrustOutcome::Failed => rust_i18n::t!("library_root_trust.failed_toast"),
        };
        self.add_toast(message.as_ref());

        // A stale, failed, or incomplete attempt grants no active authority.
        // Release both layers of lifetime deduplication so a refreshed event
        // with unchanged evidence can be presented and completed later.
        if outcome_allows_retry(outcome) {
            self.inner.queue.borrow_mut().allow_retry(&request_id);
            self.inner.finished_ids.borrow_mut().remove(&request_id);
        }
    }

    fn present_next(&self) {
        if !self.inner.command_admission.is_open() {
            return;
        }
        let Some(prompt) = self.inner.queue.borrow_mut().take_next() else {
            return;
        };

        let copy = prompt_copy(&prompt.payload);
        let dialog = adw::AlertDialog::builder()
            .heading(&copy.heading)
            .body(&copy.body)
            .close_response(DEFER_RESPONSE)
            .default_response(DEFER_RESPONSE)
            .build();
        dialog.add_response(DEFER_RESPONSE, &copy.defer_label);
        dialog.add_response(CONFIRM_RESPONSE, &copy.confirm_label);
        dialog.set_response_appearance(CONFIRM_RESPONSE, copy.appearance);

        let controller = self.clone();
        let prompt_id = prompt.id;
        let request = prompt.payload;
        dialog.connect_response(None, move |_dialog, response| {
            controller.handle_response(
                prompt_id.clone(),
                request.clone(),
                PromptStage::Initial,
                response,
            );
        });

        dialog.present(Some(&self.inner.window));
    }

    fn present_empty_final(&self, prompt_id: String, request: RootTrustRequest) {
        if !self.inner.command_admission.is_open() {
            return;
        }
        let path = request.path().display().to_string();
        let remembered = request.remembered_track_count();
        let fresh_empty = request.reason() == RootTrustReason::EmptyRoot && remembered == 0;
        let (heading, body) = if fresh_empty {
            (
                rust_i18n::t!("library_root_trust.empty_new_final_heading"),
                rust_i18n::t!("library_root_trust.empty_new_final_body", path = path),
            )
        } else {
            (
                rust_i18n::t!("library_root_trust.empty_final_heading"),
                rust_i18n::t!(
                    "library_root_trust.empty_final_body",
                    path = path,
                    count = remembered
                ),
            )
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading.as_ref())
            .body(body.as_ref())
            .close_response(DEFER_RESPONSE)
            .default_response(DEFER_RESPONSE)
            .build();
        dialog.add_response(
            DEFER_RESPONSE,
            if fresh_empty {
                rust_i18n::t!("library_root_trust.not_now")
            } else {
                rust_i18n::t!("library_root_trust.keep_existing")
            }
            .as_ref(),
        );
        dialog.add_response(
            CONFIRM_RESPONSE,
            rust_i18n::t!("library_root_trust.trust_empty_anyway").as_ref(),
        );
        dialog.set_response_appearance(CONFIRM_RESPONSE, adw::ResponseAppearance::Destructive);

        let controller = self.clone();
        dialog.connect_response(None, move |_dialog, response| {
            controller.handle_response(
                prompt_id.clone(),
                request.clone(),
                PromptStage::EmptyFinal,
                response,
            );
        });
        dialog.present(Some(&self.inner.window));
    }

    fn handle_response(
        &self,
        prompt_id: String,
        request: RootTrustRequest,
        stage: PromptStage,
        response: &str,
    ) {
        // Empty storage always needs the second acknowledgement, including a
        // confirmed-root replacement whose primary reason remains
        // `Replacement` for the first dialog's stronger context.
        let empty_root = request.requires_empty_acknowledgement();
        match response_action(empty_root, stage, response) {
            ResponseAction::ContinueToEmptyFinal => {
                // Keep this queue item active across both acknowledgements.
                // A duplicate response from the first dialog cannot open a
                // second final confirmation because the stage has advanced.
                if !self.inner.queue.borrow_mut().advance(
                    &prompt_id,
                    PromptStage::Initial,
                    PromptStage::EmptyFinal,
                ) {
                    return;
                }
                let controller = self.clone();
                gtk::glib::idle_add_local_once(move || {
                    controller.present_empty_final(prompt_id, request);
                });
            }
            action @ (ResponseAction::Defer | ResponseAction::Confirm) => {
                // A response signal should fire once, but guard it anyway: a
                // duplicate callback must neither confirm twice nor advance
                // two queued prompts.
                if !self.inner.queue.borrow_mut().finish(&prompt_id, stage) {
                    return;
                }

                if action == ResponseAction::Confirm {
                    if self
                        .inner
                        .command_admission
                        .try_send(LibraryCommand::ConfirmRootTrust(request))
                    {
                        self.add_toast(rust_i18n::t!("library_root_trust.pending_toast").as_ref());
                    } else {
                        // The request was not delivered, so do not turn
                        // lifetime deduplication into a permanent denial
                        // of the user's ability to retry.
                        self.inner.queue.borrow_mut().allow_retry(&prompt_id);
                        tracing::warn!("Library root trust command admission is closed");
                        self.add_toast(rust_i18n::t!("library_root_trust.failed_toast").as_ref());
                    }
                }

                // Wait until the closing dialog has unwound before presenting
                // the next modal. This guarantees one visible prompt at a time.
                let next = self.clone();
                gtk::glib::idle_add_local_once(move || next.present_next());
            }
        }
    }

    fn add_toast(&self, message: &str) {
        self.inner.toast_overlay.add_toast(adw::Toast::new(message));
    }
}

struct PromptCopy {
    heading: String,
    body: String,
    defer_label: String,
    confirm_label: String,
    appearance: adw::ResponseAppearance,
}

fn prompt_copy(request: &RootTrustRequest) -> PromptCopy {
    let path = request.path().display().to_string();
    let remembered = request.remembered_track_count();
    match request.reason() {
        RootTrustReason::LegacyEnrollment => PromptCopy {
            heading: rust_i18n::t!("library_root_trust.legacy_heading").into_owned(),
            body: rust_i18n::t!(
                "library_root_trust.legacy_body",
                path = path,
                count = remembered
            )
            .into_owned(),
            defer_label: rust_i18n::t!("library_root_trust.not_now").into_owned(),
            confirm_label: rust_i18n::t!("library_root_trust.trust_folder").into_owned(),
            appearance: adw::ResponseAppearance::Suggested,
        },
        RootTrustReason::Replacement => PromptCopy {
            heading: rust_i18n::t!("library_root_trust.replacement_heading").into_owned(),
            body: rust_i18n::t!(
                "library_root_trust.replacement_body",
                path = path,
                count = remembered
            )
            .into_owned(),
            defer_label: rust_i18n::t!("library_root_trust.keep_existing").into_owned(),
            confirm_label: rust_i18n::t!("library_root_trust.use_folder").into_owned(),
            appearance: adw::ResponseAppearance::Destructive,
        },
        RootTrustReason::EmptyRoot => PromptCopy {
            heading: if remembered == 0 {
                rust_i18n::t!("library_root_trust.empty_new_heading").into_owned()
            } else {
                rust_i18n::t!("library_root_trust.empty_heading").into_owned()
            },
            body: if remembered == 0 {
                rust_i18n::t!("library_root_trust.empty_new_body", path = path).into_owned()
            } else {
                rust_i18n::t!(
                    "library_root_trust.empty_body",
                    path = path,
                    count = remembered
                )
                .into_owned()
            },
            defer_label: if remembered == 0 {
                rust_i18n::t!("library_root_trust.not_now").into_owned()
            } else {
                rust_i18n::t!("library_root_trust.keep_existing").into_owned()
            },
            confirm_label: rust_i18n::t!("library_root_trust.review_empty_risk").into_owned(),
            appearance: adw::ResponseAppearance::Suggested,
        },
    }
}

fn response_confirms(response: &str) -> bool {
    response == CONFIRM_RESPONSE
}

fn outcome_allows_retry(outcome: RootTrustOutcome) -> bool {
    outcome != RootTrustOutcome::Active
}

/// Translate a dialog response into a guarded state transition.
///
/// Unknown responses always fail closed. Empty-root requests require both the
/// initial acknowledgement and the separate destructive confirmation before
/// they may emit a command.
fn response_action(empty_root: bool, stage: PromptStage, response: &str) -> ResponseAction {
    if !response_confirms(response) {
        return ResponseAction::Defer;
    }

    match (empty_root, stage) {
        (true, PromptStage::Initial) => ResponseAction::ContinueToEmptyFinal,
        (true, PromptStage::EmptyFinal) | (false, PromptStage::Initial) => ResponseAction::Confirm,
        // A non-empty request can never legitimately reach the second stage.
        (false, PromptStage::EmptyFinal) => ResponseAction::Defer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_and_allows_only_one_active_prompt() {
        let mut queue = PromptQueue::new();
        assert!(queue.enqueue("first".to_string(), 1));
        assert!(queue.enqueue("second".to_string(), 2));

        let first = queue.take_next().expect("first prompt");
        assert_eq!(first.id, "first");
        assert_eq!(first.payload, 1);
        assert!(queue.take_next().is_none());

        assert!(queue.finish("first", PromptStage::Initial));
        let second = queue.take_next().expect("second prompt");
        assert_eq!(second.id, "second");
        assert_eq!(second.payload, 2);
    }

    #[test]
    fn request_ids_are_deduplicated_for_the_queue_lifetime() {
        let mut queue = PromptQueue::new();
        assert!(queue.enqueue("same".to_string(), 1));
        assert!(!queue.enqueue("same".to_string(), 2));
        let prompt = queue.take_next().expect("prompt");
        assert_eq!(prompt.payload, 1);
        assert!(!queue.enqueue("same".to_string(), 3));
        assert!(queue.finish("same", PromptStage::Initial));
        assert!(!queue.enqueue("same".to_string(), 4));
    }

    #[test]
    fn a_stale_completion_cannot_advance_the_queue() {
        let mut queue = PromptQueue::new();
        assert!(queue.enqueue("current".to_string(), 1));
        let _ = queue.take_next().expect("current prompt");
        assert!(!queue.finish("stale", PromptStage::Initial));
        assert!(queue.take_next().is_none());
        assert!(queue.finish("current", PromptStage::Initial));
    }

    #[test]
    fn empty_root_remains_active_across_both_stages() {
        let mut queue = PromptQueue::new();
        assert!(queue.enqueue("empty".to_string(), 1));
        assert!(queue.enqueue("next".to_string(), 2));
        let prompt = queue.take_next().expect("empty prompt");
        assert_eq!(prompt.id, "empty");

        assert!(queue.advance("empty", PromptStage::Initial, PromptStage::EmptyFinal));
        assert!(queue.take_next().is_none());
        assert!(!queue.advance("empty", PromptStage::Initial, PromptStage::EmptyFinal));
        assert!(!queue.finish("empty", PromptStage::Initial));
        assert!(queue.finish("empty", PromptStage::EmptyFinal));

        assert_eq!(queue.take_next().expect("next prompt").id, "next");
    }

    #[test]
    fn failed_command_delivery_allows_the_engine_to_requeue_the_request() {
        let mut queue = PromptQueue::new();
        assert!(queue.enqueue("retry".to_string(), 1));
        let _ = queue.take_next().expect("prompt");
        assert!(queue.finish("retry", PromptStage::Initial));
        assert!(!queue.enqueue("retry".to_string(), 2));

        queue.allow_retry("retry");
        assert!(queue.enqueue("retry".to_string(), 3));
    }

    #[test]
    fn only_the_exact_affirmative_response_confirms() {
        assert!(response_confirms(CONFIRM_RESPONSE));
        for response in [DEFER_RESPONSE, "close", "cancel", "", "CONFIRM"] {
            assert!(
                !response_confirms(response),
                "{response:?} must fail closed"
            );
        }
    }

    #[test]
    fn empty_root_requires_two_exact_affirmative_responses() {
        assert_eq!(
            response_action(true, PromptStage::Initial, CONFIRM_RESPONSE),
            ResponseAction::ContinueToEmptyFinal
        );
        assert_eq!(
            response_action(true, PromptStage::EmptyFinal, CONFIRM_RESPONSE),
            ResponseAction::Confirm
        );

        for stage in [PromptStage::Initial, PromptStage::EmptyFinal] {
            for response in [DEFER_RESPONSE, "close", "cancel", "", "CONFIRM"] {
                assert_eq!(
                    response_action(true, stage, response),
                    ResponseAction::Defer,
                    "{response:?} at {stage:?} must fail closed"
                );
            }
        }
    }

    #[test]
    fn non_empty_requests_confirm_once_and_reject_an_impossible_second_stage() {
        assert_eq!(
            response_action(false, PromptStage::Initial, CONFIRM_RESPONSE),
            ResponseAction::Confirm
        );
        assert_eq!(
            response_action(false, PromptStage::EmptyFinal, CONFIRM_RESPONSE),
            ResponseAction::Defer
        );
    }

    #[test]
    fn only_active_outcomes_remain_lifetime_deduplicated() {
        assert!(!outcome_allows_retry(RootTrustOutcome::Active));
        for outcome in [
            RootTrustOutcome::TrustedButUnavailable,
            RootTrustOutcome::Stale,
            RootTrustOutcome::Failed,
        ] {
            assert!(outcome_allows_retry(outcome));
        }
    }
}
