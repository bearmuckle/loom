use super::*;

impl InProcessConnection {
    pub(super) fn terminal_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Terminal(TerminalRequest::OpenSessionTerminal {
                session_id,
                command,
                args,
                cwd,
            }) => {
                let filesystem = self.session_filesystem(session_id)?;
                let cwd = filesystem.directory_path(cwd.as_deref().unwrap_or("."))?;
                let snapshot = self.backend.terminals.open(command, args, cwd)?;
                self.backend
                    .session_terminals()?
                    .insert(snapshot.id, session_id);
                Ok(ServerResponse::Terminal(TerminalResponse::TerminalOpened(
                    snapshot,
                )))
            }
            ClientRequest::Terminal(TerminalRequest::WriteSessionTerminalInput {
                session_id,
                terminal_id,
                input,
            }) => {
                self.check_terminal_session(session_id, terminal_id)?;
                self.backend.terminals.write_input(terminal_id, &input)?;
                Ok(ServerResponse::Terminal(TerminalResponse::Terminal(
                    self.backend.terminals.get(terminal_id)?,
                )))
            }
            ClientRequest::Terminal(TerminalRequest::ResizeSessionTerminal {
                session_id,
                terminal_id,
                rows,
                columns,
            }) => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(TerminalResponse::Terminal(
                    self.backend.terminals.resize(terminal_id, rows, columns)?,
                )))
            }
            ClientRequest::Terminal(TerminalRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence,
            }) => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(TerminalResponse::TerminalEvents {
                    events: self
                        .backend
                        .terminals
                        .events_since(terminal_id, after_sequence)?,
                }))
            }
            ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal {
                session_id,
                terminal_id,
            }) => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(TerminalResponse::Terminal(
                    self.backend.terminals.cancel(terminal_id)?,
                )))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
