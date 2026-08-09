# Email report setup

Metasys Summary sends multipart plain-text and HTML reports through an existing SMTP relay. It does not operate an email server and does not support unencrypted SMTP.

## Configuration

Open the dashboard at `http://127.0.0.1:3030` on the host Mac and select **Reports**.

1. Enter the SMTP hostname, port, username, sender name, and sender address.
2. Select STARTTLS (commonly port 587) or implicit TLS (commonly port 465), according to the relay provider.
3. Enter the SMTP password. It is stored only in macOS Keychain.
4. Add up to 50 recipient addresses.
5. Choose daily, weekday, or weekly delivery and a local send time.
6. Select at least one report section and save.
7. Use **Test SMTP**, then **Send report now**.

The sender account needs permission to use the configured `From` address. Some providers require SMTP AUTH, an application password, or a dedicated relay connector. Follow the provider's current security policy; do not reuse a Metasys or administrator password.

## Report sections

- Active alarms
- Most common alarms over the indexed 30-day history
- Most serious alarms
- Active operator overrides
- Most problematic equipment
- Equipment offline or communication failures
- Fourteen-day alarm rate and seven-day mean

The offline-equipment section is inferred from active alarm type, point, and message text containing offline, unreachable, not responding, or communication/device-failure conditions. It should be validated against site-specific alarm naming.

## Scheduling

The app checks the schedule once per minute. A successful report is sent at most once per local calendar day. Failed scheduled deliveries can retry after 15 minutes. Delivery outcomes are stored in the local SQLite database; message bodies and SMTP passwords are not stored.
