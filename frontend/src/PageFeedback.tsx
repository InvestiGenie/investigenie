export function ErrorAlert({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  return <div className="alert" role="alert"><span className="alert-symbol">!</span><span>{message}</span><button aria-label="Dismiss error" onClick={onDismiss}>{'\u00d7'}</button></div>;
}

export function StatusNotice({ message }: { message: string }) {
  return <div className="connection-notice" role="status">{message}</div>;
}
