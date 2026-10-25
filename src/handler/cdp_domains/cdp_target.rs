use cdp_browser_lite::{CdpClient, CdpResult, EventFilter, Tab, WsResponse};
use serde::Serialize;

#[derive(Clone)]
pub(crate) enum CdpTarget {
    Client(CdpClient),
    Tab(Tab),
}

impl std::fmt::Debug for CdpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(_) => f.write_str("CdpTarget::Client"),
            Self::Tab(t) => write!(f, "CdpTarget::Tab(target_id: {})", t.target_id()),
        }
    }
}

impl CdpTarget {
    /// Sends a command. A JavaScript dialog the page opens meanwhile blocks
    /// the page, so the command would wait for a timeout: it fails at once
    /// instead, with a `ProtocolError` under `dialogs::DIALOG_ERROR_CODE`
    /// whose message quotes the dialog.
    pub(crate) async fn send_raw_command<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> CdpResult<WsResponse> {
        let send = async {
            match self {
                Self::Client(c) => c.send_raw_command(method, params).await,
                Self::Tab(t) => t.send_raw_command(method, params).await,
            }
        };
        if method == "Page.handleJavaScriptDialog" {
            return send.await;
        }
        let opened = |event: &CdpResult<WsResponse>| match event {
            Ok(event) if event.method.as_deref() == Some("Page.javascriptDialogOpening") => event
                .params
                .as_ref()
                .map(crate::handler::dialogs::OpenDialog::from_chrome),
            _ => None,
        };
        crate::handler::dialogs::unless_dialog(send, self.on_domain("Page"), opened)
            .await
            .unwrap_or_else(|dialog| {
                Err(cdp_browser_lite::CdpError::ProtocolError {
                    code: crate::handler::dialogs::DIALOG_ERROR_CODE,
                    message: dialog.opened_during(method),
                })
            })
    }

    pub(crate) fn on_domain(&self, domain: &'static str) -> EventFilter {
        match self {
            Self::Client(c) => c.on_domain(domain),
            Self::Tab(t) => t.on_domain(domain),
        }
    }
}
