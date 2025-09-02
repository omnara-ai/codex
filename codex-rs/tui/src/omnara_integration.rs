use codex_core::omnara_client::OmnaraClient;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use tokio::task::JoinHandle;

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

        let handle = tokio::spawn(async move {
            info!("OmnaraBridge: sending agent message");
            client.append_log("[Bridge] sending agent message via client\n");
            let _ = client.send_agent_message(&message, false).await;
            if request_after {
                // Deterministically request input on the last sent message and begin polling.
                info!("OmnaraBridge: requesting user input after agent message");
                client.append_log("[Bridge] request_user_input_for_last_message\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx);
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
            tokio::spawn(async move {
                let _ = handle.await;
                info!("OmnaraBridge: last agent send completed; requesting user input");
                client.append_log("[Bridge] awaiting last send complete\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx);
            });
        } else {
            let client = self.client.clone();
            let app_event_tx = self.app_event_tx.clone();
            let codex_op_tx = self.codex_op_tx.clone();
            tokio::spawn(async move {
                info!("OmnaraBridge: no last send; requesting user input now");
                client.append_log("[Bridge] no last send; request input\n");
                let _ = client.request_user_input_for_last_message().await;
                Self::start_polling_impl(client, app_event_tx, codex_op_tx);
            });
        }
    }

    /// Send the standard interrupt message (requires input) and start polling immediately.
    pub fn on_user_interrupt(&mut self) {
        info!("OmnaraBridge.on_user_interrupt");
        self.client.append_log("[Bridge] on_user_interrupt\n");
        let client = self.client.clone();
        let app_event_tx = self.app_event_tx.clone();
        let codex_op_tx = self.codex_op_tx.clone();
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
            Self::start_polling_impl(client, app_event_tx, codex_op_tx);
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
    ) {
        info!("OmnaraBridge: starting polling loop");
        client.start_polling(move |text: String| {
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
            Self::start_polling_impl(client, app_event_tx, codex_op_tx);
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
}
