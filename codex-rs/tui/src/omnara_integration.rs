use codex_core::omnara_client::OmnaraClient;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use tokio::task::JoinHandle;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell;
use tracing::{debug, info};

/// Thin TUI-side bridge over the core Omnara client.
/// - Tracks last agent send handle so we can request input deterministically.
/// - Starts polling and forwards remote user messages into the UI and agent.
pub(crate) struct OmnaraBridge {
    client: OmnaraClient,
    last_agent_send_handle: Option<JoinHandle<()>>,
    app_event_tx: AppEventSender,
    codex_op_tx: tokio::sync::mpsc::UnboundedSender<Op>,
    pending: Arc<Mutex<VecDeque<(String, ApprovalKind)>>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ApprovalKind {
    Exec,
    Patch,
}

impl OmnaraBridge {
    pub fn new(
        client: OmnaraClient,
        app_event_tx: AppEventSender,
        codex_op_tx: tokio::sync::mpsc::UnboundedSender<Op>,
    ) -> Self {
        info!(session_id = %client.session_id(), "OmnaraBridge: enabled");
        Self {
            client,
            last_agent_send_handle: None,
            app_event_tx,
            codex_op_tx,
            pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn from_env(app_event_tx: AppEventSender, codex_op_tx: tokio::sync::mpsc::UnboundedSender<Op>) -> Option<Self> {
        match OmnaraClient::from_env() {
            Some(client) => Some(Self::new(client, app_event_tx, codex_op_tx)),
            None => {
                debug!("OmnaraBridge: disabled (no API key)");
                None
            }
        }
    }

    /// Send agent message (no input required). If `request_after` is true, we
    /// will request user input and start polling after the send completes.
    pub fn on_agent_message(&mut self, message: String, request_after: bool) {
        debug!(request_after, "OmnaraBridge.on_agent_message");
        self.client.append_log(&format!(
            "[Bridge] on_agent_message(request_after={})\n",
            request_after
        ));
        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
        let pending = self.pending.clone();

        let handle = tokio::spawn(async move {
            info!("OmnaraBridge: sending agent message");
            client.append_log("[Bridge] sending agent message via client\n");
            let _ = client.send_agent_message(&message, false).await;
            if request_after {
                // Deterministically request input on the last sent message and begin polling.
                info!("OmnaraBridge: requesting user input after agent message");
                client.append_log("[Bridge] request_user_input_for_last_message\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx, pending);
            }
        });

        self.last_agent_send_handle = Some(handle);
    }

    /// Called when Codex signals a task completed. Await the last send (if any),
    /// then request user input and start polling.
    pub fn on_task_complete(&mut self) {
        info!("OmnaraBridge.on_task_complete");
        self.client.append_log("[Bridge] on_task_complete\n");
        if let Some(handle) = self.last_agent_send_handle.take() {
            let client = self.client.clone();
            let app_event_tx = self.app_event_tx.clone();
            let codex_op_tx = self.codex_op_tx.clone();
            let pending = self.pending.clone();
            tokio::spawn(async move {
                let _ = handle.await;
                info!("OmnaraBridge: last agent send completed; requesting user input");
                client.append_log("[Bridge] awaiting last send complete\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx, pending);
            });
        } else {
            let client = self.client.clone();
            let app_event_tx = self.app_event_tx.clone();
            let codex_op_tx = self.codex_op_tx.clone();
            let pending = self.pending.clone();
            tokio::spawn(async move {
                info!("OmnaraBridge: no last send; requesting user input now");
                client.append_log("[Bridge] no last send; request input\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx, pending);
            });
        }
    }

    /// Send the standard interrupt message (requires input) and start polling immediately.


    /// Send a plain agent note to Omnara (no user input required).
    pub fn send_note(&self, message: String) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.send_agent_message(&message, false).await;
        });
    }
    pub fn on_user_interrupt(&mut self) {
        info!("OmnaraBridge.on_user_interrupt");
        self.client.append_log("[Bridge] on_user_interrupt\n");
        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            if let Ok(id) = client
                .send_agent_message("Tell the model what to do differently", true)
                .await
            {
                client.set_last_read_message_id(id);
            }
            // No need to request input again; the send above already did requires_user_input.
            info!("OmnaraBridge: interrupt sent; starting polling");
            client.append_log("[Bridge] interrupt sent; start polling\n");
            Self::start_polling_impl(client, app_event_tx, codex_op_tx, pending);
        });
    }

    /// Cancel any active poll (called when local user submits input).
    pub fn cancel_polling(&self) {
        debug!("OmnaraBridge.cancel_polling");
        self.client.append_log("[Bridge] cancel_polling\n");
        self.client.cancel_polling();
    }

    /// Mirror a local user message to Omnara as a USER message, marking it as read.
    pub fn on_local_user_message(&self, text: String) {
        info!(len = text.len(), "OmnaraBridge.on_local_user_message");
        self.client.append_log("[Bridge] on_local_user_message\n");
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.send_user_message(&text, true).await;
        });
    }

    fn start_polling_impl(
        client: OmnaraClient,
        app_event_tx: AppEventSender,
        codex_op_tx: tokio::sync::mpsc::UnboundedSender<Op>,
        pending: Arc<Mutex<VecDeque<(String, ApprovalKind)>>>,
    ) {
        info!("OmnaraBridge: starting polling loop");
        client.start_polling(move |text: String| {
            if let Some(decision) = parse_approval_response(&text) {
                if let Ok(mut q) = pending.lock() {
                    if let Some((_id, _kind)) = q.pop_front() {
                        // Resolve the modal in UI; this will also send the op.
                        app_event_tx.send(AppEvent::ResolveApproval { decision });
                        return;
                    }
                }
            } else {
                // Fallback: if an approval is pending but response text does not match
                // a known option, treat it as a rejection (Abort).
                if let Ok(mut q) = pending.lock() {
                    if let Some((_id, _kind)) = q.pop_front() {
                        app_event_tx.send(AppEvent::ResolveApproval { decision: codex_core::protocol::ReviewDecision::Abort });
                        return;
                    }
                }
            }
            // 1) Show in TUI history like a user-typed message.
            app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                history_cell::new_user_prompt(text.clone()),
            )));

            // 2) Send to the agent as user input.
            let _ = codex_op_tx.send(Op::UserInput {
                items: vec![InputItem::Text { text: text.clone() }],
            });
            let _ = codex_op_tx.send(Op::AddToHistory { text });
        });
    }

    /// On startup, publish a session start notice (requires input) and begin polling.
    pub fn on_session_start(&mut self) {
        info!("OmnaraBridge.on_session_start");
        self.client.append_log("[Bridge] on_session_start\n");
        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            if let Ok(id) = client
                .send_agent_message(
                    "Codex session started - waiting for your input...",
                    true,
                )
                .await
            {
                client.set_last_read_message_id(id);
            }
            Self::start_polling_impl(client, app_event_tx, codex_op_tx, pending);
        });
    }

    /// On shutdown, end the Omnara session and return a JoinHandle to await.
    pub fn on_session_end(&self) -> tokio::task::JoinHandle<()> {
        info!("OmnaraBridge.on_session_end");
        self.client.append_log("[Bridge] on_session_end\n");
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.end_session().await;
        })
    }

    /// Send an approval request to Omnara (exec) and start polling.
    pub fn send_exec_approval_request(
        &mut self,
        request_id: String,
        command: Vec<String>,
        reason: Option<String>,
    ) {
        let approval_msg = crate::omnara_format::format_exec_approval_request(&command, reason.as_deref());
        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            if let Ok(id) = client
                .send_agent_message(&approval_msg, true)
                .await
            {
                client.set_last_read_message_id(id);
                client.append_log(&format!(
                    "Sent exec approval request - Request ID: {}\n",
                    request_id
                ));
                if let Ok(mut q) = pending.lock() { q.push_back((request_id, ApprovalKind::Exec)); }
                OmnaraBridge::start_polling_impl(client, app_event_tx, codex_op_tx, pending.clone());
            }
        });
    }

    /// Send an approval request to Omnara (patch) and start polling.
    pub fn send_patch_approval_request(
        &mut self,
        request_id: String,
        file_count: usize,
        added_lines: usize,
        removed_lines: usize,
        reason: Option<String>,
        grant_root: Option<std::path::PathBuf>,
        patch_details: Option<String>,
    ) {
        let approval_msg = crate::omnara_format::format_patch_approval_request(
            file_count,
            added_lines,
            removed_lines,
            reason.as_deref(),
            grant_root.as_deref(),
            patch_details.as_deref(),
        );

        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            if let Ok(id) = client
                .send_agent_message(&approval_msg, true)
                .await
            {
                client.set_last_read_message_id(id);
                client.append_log(&format!(
                    "Sent patch approval request - Request ID: {}\n",
                    request_id
                ));
                if let Ok(mut q) = pending.lock() { q.push_back((request_id, ApprovalKind::Patch)); }
                OmnaraBridge::start_polling_impl(client, app_event_tx, codex_op_tx, pending.clone());
            }
        });
    }
}

fn parse_approval_response(message: &str) -> Option<codex_core::protocol::ReviewDecision> {
    let normalized = message.trim().to_lowercase();
    if normalized == "yes" {
        Some(codex_core::protocol::ReviewDecision::Approved)
    } else if normalized == "always" {
        Some(codex_core::protocol::ReviewDecision::ApprovedForSession)
    } else if normalized == "no, provide feedback" || normalized == "no" {
        Some(codex_core::protocol::ReviewDecision::Abort)
    } else {
        None
    }
}
