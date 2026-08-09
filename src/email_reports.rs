use std::{collections::BTreeSet, fmt::Write as _, time::Duration as StdDuration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, Utc};
use keyring::Entry;
use lettre::{
    Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::authentication::Credentials,
};
use serde::{Deserialize, Serialize};

use crate::models::DashboardView;

const SMTP_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard.smtp";
const SMTP_KEYCHAIN_ACCOUNT: &str = "report-sender";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SmtpTlsMode {
    StartTls,
    ImplicitTls,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReportCadence {
    Daily,
    Weekdays,
    Weekly,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportSections {
    pub active_alarms: bool,
    pub common_alarms: bool,
    pub serious_alarms: bool,
    pub operator_overrides: bool,
    pub problematic_equipment: bool,
    pub equipment_offline: bool,
    pub alarm_rate: bool,
}

impl Default for ReportSections {
    fn default() -> Self {
        Self {
            active_alarms: true,
            common_alarms: true,
            serious_alarms: true,
            operator_overrides: true,
            problematic_equipment: true,
            equipment_offline: true,
            alarm_rate: true,
        }
    }
}

impl ReportSections {
    fn any_enabled(&self) -> bool {
        self.active_alarms
            || self.common_alarms
            || self.serious_alarms
            || self.operator_overrides
            || self.problematic_equipment
            || self.equipment_offline
            || self.alarm_rate
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailReportSettings {
    pub enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub from_name: String,
    pub from_address: String,
    pub tls_mode: SmtpTlsMode,
    pub recipients: Vec<String>,
    pub cadence: ReportCadence,
    pub send_time: String,
    pub weekly_day: u32,
    pub sections: ReportSections,
}

impl Default for EmailReportSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            from_name: "Metasys Summary".to_owned(),
            from_address: String::new(),
            tls_mode: SmtpTlsMode::StartTls,
            recipients: Vec::new(),
            cadence: ReportCadence::Daily,
            send_time: "08:00".to_owned(),
            weekly_day: 1,
            sections: ReportSections::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailReportSettingsUpdate {
    pub enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub from_name: String,
    pub from_address: String,
    pub tls_mode: SmtpTlsMode,
    pub recipients: Vec<String>,
    pub cadence: ReportCadence,
    pub send_time: String,
    pub weekly_day: u32,
    pub sections: ReportSections,
    #[serde(default)]
    pub smtp_password: Option<String>,
    #[serde(default)]
    pub clear_password: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailReportSettingsView {
    pub enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub from_name: String,
    pub from_address: String,
    pub tls_mode: SmtpTlsMode,
    pub recipients: Vec<String>,
    pub cadence: ReportCadence,
    pub send_time: String,
    pub weekly_day: u32,
    pub sections: ReportSections,
    pub password_configured: bool,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailDeliveryResult {
    pub status: String,
    pub recipient_count: usize,
    pub sent_at: DateTime<Utc>,
}

impl EmailReportSettingsUpdate {
    pub fn validated_settings(&self) -> Result<EmailReportSettings> {
        if self.clear_password
            && self
                .smtp_password
                .as_ref()
                .is_some_and(|password| !password.is_empty())
        {
            bail!("smtpPassword and clearPassword cannot be supplied together");
        }
        let mut seen = BTreeSet::new();
        let recipients = self
            .recipients
            .iter()
            .map(|address| address.trim().to_ascii_lowercase())
            .filter(|address| !address.is_empty())
            .filter(|address| seen.insert(address.clone()))
            .collect::<Vec<_>>();
        let settings = EmailReportSettings {
            enabled: self.enabled,
            smtp_host: self.smtp_host.trim().to_owned(),
            smtp_port: self.smtp_port,
            smtp_username: self.smtp_username.trim().to_owned(),
            from_name: self.from_name.trim().to_owned(),
            from_address: self.from_address.trim().to_ascii_lowercase(),
            tls_mode: self.tls_mode,
            recipients,
            cadence: self.cadence,
            send_time: self.send_time.trim().to_owned(),
            weekly_day: self.weekly_day,
            sections: self.sections.clone(),
        };
        settings.validate_for_storage()?;
        Ok(settings)
    }
}

impl EmailReportSettings {
    pub fn validate(&self) -> Result<()> {
        if self.smtp_port == 0 {
            bail!("SMTP port must be between 1 and 65535");
        }
        validate_text("SMTP host", &self.smtp_host, 253)?;
        if self.smtp_host.contains("//")
            || self.smtp_host.contains('/')
            || self.smtp_host.contains('\\')
        {
            bail!("SMTP host must be a hostname or IP address, without a URL scheme");
        }
        validate_text("SMTP username", &self.smtp_username, 256)?;
        validate_text("sender name", &self.from_name, 128)?;
        parse_address("sender address", &self.from_address)?;
        if self.recipients.is_empty() || self.recipients.len() > 50 {
            bail!("provide between 1 and 50 report recipients");
        }
        for recipient in &self.recipients {
            parse_address("recipient", recipient)?;
        }
        scheduled_time(self)?;
        if !(1..=7).contains(&self.weekly_day) {
            bail!("weekly day must be between 1 (Monday) and 7 (Sunday)");
        }
        if !self.sections.any_enabled() {
            bail!("select at least one report section");
        }
        Ok(())
    }

    fn validate_for_storage(&self) -> Result<()> {
        let has_configuration = !self.smtp_host.is_empty()
            || !self.smtp_username.is_empty()
            || !self.from_address.is_empty()
            || !self.recipients.is_empty();
        if self.enabled || has_configuration {
            self.validate()
        } else {
            scheduled_time(self)?;
            if !self.sections.any_enabled() {
                bail!("select at least one report section");
            }
            Ok(())
        }
    }

    pub fn view(
        &self,
        last_attempt_at: Option<DateTime<Utc>>,
        last_success_at: Option<DateTime<Utc>>,
        last_error: Option<String>,
    ) -> EmailReportSettingsView {
        EmailReportSettingsView {
            enabled: self.enabled,
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port,
            smtp_username: self.smtp_username.clone(),
            from_name: self.from_name.clone(),
            from_address: self.from_address.clone(),
            tls_mode: self.tls_mode,
            recipients: self.recipients.clone(),
            cadence: self.cadence,
            send_time: self.send_time.clone(),
            weekly_day: self.weekly_day,
            sections: self.sections.clone(),
            password_configured: smtp_password_configured(),
            last_attempt_at,
            last_success_at,
            last_error,
        }
    }

    pub fn is_due(
        &self,
        now: DateTime<Local>,
        last_attempt_at: Option<DateTime<Utc>>,
        last_success_at: Option<DateTime<Utc>>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let Ok(send_time) = scheduled_time(self) else {
            return false;
        };
        if now.time() < send_time {
            return false;
        }
        let weekday = now.weekday().number_from_monday();
        let scheduled_today = match self.cadence {
            ReportCadence::Daily => true,
            ReportCadence::Weekdays => weekday <= 5,
            ReportCadence::Weekly => weekday == self.weekly_day,
        };
        if !scheduled_today {
            return false;
        }
        if last_success_at
            .map(|timestamp| timestamp.with_timezone(&Local).date_naive() == now.date_naive())
            .unwrap_or(false)
        {
            return false;
        }
        !last_attempt_at
            .map(|timestamp| now.with_timezone(&Utc) - timestamp < Duration::minutes(15))
            .unwrap_or(false)
    }
}

pub fn set_smtp_password(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("SMTP password cannot be empty");
    }
    smtp_keychain_entry()?
        .set_password(password)
        .context("save SMTP password in macOS Keychain")
}

pub fn clear_smtp_password() -> Result<()> {
    if !smtp_password_configured() {
        return Ok(());
    }
    smtp_keychain_entry()?
        .delete_credential()
        .context("remove SMTP password from macOS Keychain")
}

pub async fn test_smtp(settings: &EmailReportSettings) -> Result<()> {
    settings.validate()?;
    let mailer = smtp_transport(settings)?;
    let connected = mailer
        .test_connection()
        .await
        .context("test SMTP connection")?;
    if !connected {
        bail!("SMTP server rejected the connection test");
    }
    Ok(())
}

pub async fn send_report(
    settings: &EmailReportSettings,
    dashboard: &DashboardView,
) -> Result<EmailDeliveryResult> {
    settings.validate()?;
    let (plain, html) = render_report(settings, dashboard);
    let sender = Mailbox::new(
        Some(settings.from_name.clone()),
        parse_address("sender address", &settings.from_address)?,
    );
    let mut builder = Message::builder().from(sender).subject(format!(
        "Metasys Summary — {}",
        Local::now().format("%Y-%m-%d")
    ));
    for recipient in &settings.recipients {
        builder = builder.to(Mailbox::new(None, parse_address("recipient", recipient)?));
    }
    let message = builder
        .multipart(MultiPart::alternative_plain_html(plain, html))
        .context("build report email")?;
    smtp_transport(settings)?
        .send(message)
        .await
        .context("send report through SMTP")?;
    Ok(EmailDeliveryResult {
        status: "sent".to_owned(),
        recipient_count: settings.recipients.len(),
        sent_at: Utc::now(),
    })
}

fn smtp_transport(settings: &EmailReportSettings) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let password = smtp_password()?;
    let builder = match settings.tls_mode {
        SmtpTlsMode::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.smtp_host)
        }
        SmtpTlsMode::ImplicitTls => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&settings.smtp_host)
        }
    }
    .context("configure encrypted SMTP relay")?;
    Ok(builder
        .port(settings.smtp_port)
        .timeout(Some(StdDuration::from_secs(20)))
        .credentials(Credentials::new(settings.smtp_username.clone(), password))
        .build())
}

fn render_report(settings: &EmailReportSettings, dashboard: &DashboardView) -> (String, String) {
    let mut plain = format!(
        "METASYS SUMMARY\nGenerated: {}\nActive alarms: {} | Critical: {} | Overrides: {} | 30-day alarms: {}\n",
        dashboard
            .generated_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M %Z"),
        dashboard.active_alarm_count,
        dashboard.critical_active_count,
        dashboard.override_count,
        dashboard.thirty_day_alarm_count
    );
    let mut html = format!(
        "<!doctype html><html><body style=\"margin:0;padding:24px;background:#f3f6f7;color:#17313b;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif\"><div style=\"max-width:900px;margin:auto;background:#fff;border:1px solid #dce5e8;border-radius:12px;overflow:hidden\"><div style=\"padding:22px 26px;background:#0d2530;color:#fff\"><div style=\"font-size:11px;letter-spacing:.12em;color:#5dd2da\">METASYS</div><h1 style=\"margin:5px 0 3px;font-size:25px\">Operations summary</h1><div style=\"font-size:12px;color:#aac0c8\">{}</div></div><div style=\"padding:20px 26px\"><table role=\"presentation\" style=\"width:100%;border-collapse:collapse;margin-bottom:22px\"><tr>{}</tr></table>",
        escape_html(&dashboard.generated_at.with_timezone(&Local).format("%Y-%m-%d %H:%M %Z").to_string()),
        [
            ("Active alarms", dashboard.active_alarm_count.to_string()),
            ("Critical", dashboard.critical_active_count.to_string()),
            ("Overrides", dashboard.override_count.to_string()),
            ("30-day alarms", dashboard.thirty_day_alarm_count.to_string()),
        ]
        .into_iter()
        .map(|(label, value)| format!("<td style=\"padding:12px;background:#f4f8f9;border:4px solid #fff;text-align:center\"><strong style=\"display:block;font-size:22px;color:#163a46\">{}</strong><span style=\"font-size:10px;color:#69818a;text-transform:uppercase\">{}</span></td>", escape_html(&value), escape_html(label)))
        .collect::<String>()
    );

    if settings.sections.active_alarms {
        append_heading(&mut plain, &mut html, "Active alarms");
        append_alarm_rows(&mut plain, &mut html, &dashboard.active_alarms, 25, false);
    }
    if settings.sections.common_alarms {
        append_heading(&mut plain, &mut html, "Most common alarms — 30 days");
        append_alarm_rows(&mut plain, &mut html, &dashboard.frequent_alarms, 10, true);
    }
    if settings.sections.serious_alarms {
        append_heading(&mut plain, &mut html, "Most serious alarms");
        append_alarm_rows(&mut plain, &mut html, &dashboard.serious_alarms, 10, false);
    }
    if settings.sections.operator_overrides {
        append_heading(&mut plain, &mut html, "Active operator overrides");
        let rows = dashboard
            .overrides
            .iter()
            .take(25)
            .map(|item| {
                vec![
                    item.equipment.clone(),
                    item.point.clone(),
                    item.value.clone(),
                    item.status.clone(),
                ]
            })
            .collect::<Vec<_>>();
        append_table(
            &mut plain,
            &mut html,
            &["Equipment", "Point", "Value", "Status"],
            &rows,
        );
    }
    if settings.sections.problematic_equipment {
        append_heading(&mut plain, &mut html, "Most problematic equipment");
        let rows = dashboard
            .problematic_equipment
            .iter()
            .take(10)
            .map(|item| {
                vec![
                    item.equipment.clone(),
                    item.alarm_count.to_string(),
                    item.active_count.to_string(),
                    format!("{:.1}", item.score),
                ]
            })
            .collect::<Vec<_>>();
        append_table(
            &mut plain,
            &mut html,
            &["Equipment", "Alarms", "Active", "Risk score"],
            &rows,
        );
    }
    if settings.sections.equipment_offline {
        append_heading(
            &mut plain,
            &mut html,
            "Equipment offline / communication failures",
        );
        let equipment = offline_equipment(dashboard);
        let rows = equipment
            .into_iter()
            .map(|item| vec![item])
            .collect::<Vec<_>>();
        append_table(&mut plain, &mut html, &["Equipment"], &rows);
    }
    if settings.sections.alarm_rate {
        append_heading(&mut plain, &mut html, "Alarm rate — 14 days");
        let rows = dashboard
            .alarm_rate
            .iter()
            .map(|point| {
                vec![
                    point.date.to_string(),
                    point.count.to_string(),
                    format!("{:.1}", point.rolling_average),
                ]
            })
            .collect::<Vec<_>>();
        append_table(
            &mut plain,
            &mut html,
            &["Date", "Alarms", "7-day mean"],
            &rows,
        );
    }

    html.push_str("</div><div style=\"padding:14px 26px;background:#f4f8f9;color:#718991;font-size:10px\">Generated by Metasys Summary · Read-only monitoring</div></div></body></html>");
    (plain, html)
}

fn append_alarm_rows(
    plain: &mut String,
    html: &mut String,
    alarms: &[crate::models::AlarmView],
    limit: usize,
    include_count: bool,
) {
    let rows = alarms
        .iter()
        .take(limit)
        .map(|alarm| {
            let mut row = vec![
                alarm.equipment.clone(),
                alarm.point.clone(),
                alarm.message.clone(),
                alarm.priority.to_string(),
            ];
            if include_count {
                row.push(alarm.count.unwrap_or(1).to_string());
            }
            row
        })
        .collect::<Vec<_>>();
    let mut headers = vec!["Equipment", "Point", "Condition", "Priority"];
    if include_count {
        headers.push("Count");
    }
    append_table(plain, html, &headers, &rows);
}

fn append_heading(plain: &mut String, html: &mut String, heading: &str) {
    let _ = write!(
        plain,
        "\n{}\n{}\n",
        heading.to_ascii_uppercase(),
        "-".repeat(heading.len())
    );
    let _ = write!(
        html,
        "<h2 style=\"margin:24px 0 9px;font-size:17px;color:#173b47\">{}</h2>",
        escape_html(heading)
    );
}

fn append_table(plain: &mut String, html: &mut String, headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        plain.push_str("None\n");
        html.push_str("<p style=\"margin:0;color:#758a91;font-size:12px\">None</p>");
        return;
    }
    plain.push_str(&headers.join(" | "));
    plain.push('\n');
    html.push_str(
        "<table style=\"width:100%;border-collapse:collapse;font-size:11px\"><thead><tr>",
    );
    for header in headers {
        let _ = write!(
            html,
            "<th style=\"padding:7px 8px;background:#edf3f5;border:1px solid #dce6e9;text-align:left;color:#58717a\">{}</th>",
            escape_html(header)
        );
    }
    html.push_str("</tr></thead><tbody>");
    for row in rows {
        plain.push_str(&row.join(" | "));
        plain.push('\n');
        html.push_str("<tr>");
        for value in row {
            let _ = write!(
                html,
                "<td style=\"padding:7px 8px;border:1px solid #e3eaec;color:#294751\">{}</td>",
                escape_html(value)
            );
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");
}

fn offline_equipment(dashboard: &DashboardView) -> Vec<String> {
    dashboard
        .active_alarms
        .iter()
        .filter(|alarm| {
            let condition = format!("{} {} {}", alarm.alarm_type, alarm.message, alarm.point)
                .to_ascii_lowercase();
            [
                "offline",
                "off line",
                "unreachable",
                "not responding",
                "communication failure",
                "comm failure",
                "device failure",
            ]
            .iter()
            .any(|needle| condition.contains(needle))
        })
        .map(|alarm| alarm.equipment.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn scheduled_time(settings: &EmailReportSettings) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(&settings.send_time, "%H:%M")
        .context("report send time must use HH:MM format")
}

fn validate_text(name: &str, value: &str, maximum_length: usize) -> Result<()> {
    if value.is_empty() {
        bail!("{name} is required");
    }
    if value.len() > maximum_length || value.chars().any(char::is_control) {
        bail!("{name} is invalid");
    }
    Ok(())
}

fn parse_address(name: &str, value: &str) -> Result<Address> {
    value
        .parse::<Address>()
        .with_context(|| format!("{name} is not a valid email address"))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn smtp_password_configured() -> bool {
    smtp_password().is_ok()
}

fn smtp_password() -> Result<String> {
    smtp_keychain_entry()?
        .get_password()
        .context("SMTP password is missing; save it from Report Settings")
}

fn smtp_keychain_entry() -> Result<Entry> {
    Entry::new(SMTP_KEYCHAIN_SERVICE, SMTP_KEYCHAIN_ACCOUNT)
        .context("open SMTP password entry in macOS Keychain")
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::{
        EmailReportSettings, EmailReportSettingsUpdate, ReportCadence, ReportSections, SmtpTlsMode,
        escape_html, render_report,
    };

    fn valid_update() -> EmailReportSettingsUpdate {
        EmailReportSettingsUpdate {
            enabled: true,
            smtp_host: "smtp.example.invalid".to_owned(),
            smtp_port: 587,
            smtp_username: "report_sender".to_owned(),
            from_name: "Metasys Summary".to_owned(),
            from_address: "reports@example.invalid".to_owned(),
            tls_mode: SmtpTlsMode::StartTls,
            recipients: vec![
                "OPERATIONS@example.invalid".to_owned(),
                "operations@example.invalid".to_owned(),
            ],
            cadence: ReportCadence::Daily,
            send_time: "08:00".to_owned(),
            weekly_day: 1,
            sections: ReportSections::default(),
            smtp_password: None,
            clear_password: false,
        }
    }

    #[test]
    fn validates_and_deduplicates_recipients() {
        let settings = valid_update().validated_settings().unwrap();
        assert_eq!(settings.recipients, vec!["operations@example.invalid"]);
    }

    #[test]
    fn scheduled_report_runs_once_per_day() {
        let settings = valid_update().validated_settings().unwrap();
        let now = Local.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        assert!(settings.is_due(now, None, None));
        assert!(!settings.is_due(now, None, Some(now.with_timezone(&chrono::Utc))));
    }

    #[test]
    fn disabled_empty_settings_are_storable() {
        EmailReportSettings::default()
            .validate_for_storage()
            .unwrap();
        EmailReportSettings::default().validate().unwrap_err();
    }

    #[test]
    fn escapes_report_content() {
        assert_eq!(escape_html("<AHU & '1'>"), "&lt;AHU &amp; &#39;1&#39;&gt;");
    }

    #[test]
    fn renders_selected_report_sections_and_offline_equipment() {
        let alarm = crate::models::AlarmView {
            id: "alarm-1".to_owned(),
            equipment: "AHU <North>".to_owned(),
            point: "Network Status".to_owned(),
            message: "Device offline".to_owned(),
            alarm_type: "Communication Failure".to_owned(),
            category: "HVAC".to_owned(),
            priority: 20,
            severity: "critical".to_owned(),
            occurred_at: chrono::Utc::now(),
            acknowledged: false,
            count: Some(3),
        };
        let dashboard = crate::models::DashboardView {
            generated_at: chrono::Utc::now(),
            health: crate::models::HealthView::default(),
            active_alarm_count: 1,
            critical_active_count: 1,
            override_count: 0,
            thirty_day_alarm_count: 3,
            active_alarms: vec![alarm.clone()],
            frequent_alarms: vec![alarm.clone()],
            serious_alarms: vec![alarm],
            overrides: Vec::new(),
            problematic_equipment: Vec::new(),
            alarm_rate: Vec::new(),
            alarms_by_type: Vec::new(),
            alarms_by_equipment: Vec::new(),
        };
        let (plain, html) = render_report(&EmailReportSettings::default(), &dashboard);
        assert!(plain.contains("EQUIPMENT OFFLINE"));
        assert!(plain.contains("AHU <North>"));
        assert!(html.contains("AHU &lt;North&gt;"));
        assert!(!html.contains("AHU <North>"));
    }
}
