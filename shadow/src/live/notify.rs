//! notifications.mjs `toast(title, body)`: a Windows toast as `Agent Pools`, shown through
//! WinRT's `ToastNotificationManager` from this process (Agent Pools started Windows PowerShell
//! for each one). Best effort, 10 s.

use std::time::Duration;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The toast's XML, as notifications.mjs writes it.
pub fn payload(title: &str, body: &str) -> String {
    format!(r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#, escape(title), escape(body))
}

#[cfg(windows)]
fn show(xml: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::HSTRING;
    // SAFETY: joins this (blocking-pool) thread to the multithreaded apartment; an already
    // initialized thread answers S_FALSE or RPC_E_CHANGED_MODE, and either way WinRT works.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("Agent Pools"))?.Show(&toast)
}

#[cfg(not(windows))]
fn show(_: &str) -> Result<(), String> {
    Err("toasts are Windows only".into())
}

/// Shows the toast; an error says why it did not show.
pub async fn toast(title: &str, body: &str) -> Result<(), String> {
    let xml = payload(title, body);
    match tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(move || show(&xml).map_err(|e| e.to_string()))).await {
        Ok(Ok(shown)) => shown.map_err(|e| format!("toast: {e}")),
        Ok(Err(e)) => Err(format!("toast: {e}")),
        Err(_) => Err("toast: timed out".into()),
    }
}
