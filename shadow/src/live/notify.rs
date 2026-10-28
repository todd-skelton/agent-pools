//! notifications.mjs `toast(title, body)`: a Windows toast through Windows PowerShell 5.1, which
//! loads the WinRT types without extra modules. The XML travels in an environment variable (with
//! `-Command`, PowerShell would read a trailing argument as script). Best effort, 10 s.

use std::time::Duration;

const SCRIPT: &str = "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null; \
$xml = New-Object Windows.Data.Xml.Dom.XmlDocument; \
$xml.LoadXml($env:AGENT_POOLS_TOAST); \
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Agent Pools').Show([Windows.UI.Notifications.ToastNotification]::new($xml))";

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The toast's XML, as notifications.mjs writes it.
pub fn payload(title: &str, body: &str) -> String {
    format!(r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#, escape(title), escape(body))
}

/// Shows the toast; an error says why it did not show.
pub async fn toast(title: &str, body: &str) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]).env("AGENT_POOLS_TOAST", payload(title, body)).kill_on_drop(true);
    super::machine::hide_window(&mut cmd);
    match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(format!("toast: powershell exited with {}", out.status)),
        Ok(Err(e)) => Err(format!("toast: {e}")),
        Err(_) => Err("toast: timed out".into()),
    }
}
