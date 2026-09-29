use super::*;

impl InProcessConnection {
    pub(super) fn terminal_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::OpenSessionTerminal {
                session_id,
                command,
                args,
                cwd,
            } => {
                let filesystem = self.session_filesystem(session_id)?;
                let cwd = filesystem.directory_path(cwd.as_deref().unwrap_or("."))?;
                let snapshot = self.backend.terminals.open(command, args, cwd)?;
                self.backend
                    .session_terminals()?
                    .insert(snapshot.id, session_id);
                Ok(ServerResponse::TerminalOpened(snapshot))
            }
            ClientRequest::WriteSessionTerminalInput {
                session_id,
                terminal_id,
                input,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                self.backend.terminals.write_input(terminal_id, &input)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.get(terminal_id)?,
                ))
            }
            ClientRequest::ResizeSessionTerminal {
                session_id,
                terminal_id,
                rows,
                columns,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(self.backend.terminals.resize(
                    terminal_id,
                    rows,
                    columns,
                )?))
            }
            ClientRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::TerminalEvents {
                    events: self
                        .backend
                        .terminals
                        .events_since(terminal_id, after_sequence)?,
                })
            }
            ClientRequest::CancelSessionTerminal {
                session_id,
                terminal_id,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.cancel(terminal_id)?,
                ))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
