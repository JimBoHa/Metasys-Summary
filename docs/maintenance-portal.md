# Maintenance portal setup

The maintenance portal is the root page of the Rust service. It uses the same local SQLite database and read-only Metasys connector as the operations dashboard, but has its own user accounts and permissions.

## 1. Connect Metasys and create the first administrator

Start the service and open `http://127.0.0.1:3030` in a browser running on the host Mac. In step 1, enter the Metasys server URL, username, password, connector, API version, legacy domain, and certificate policy. The app fetches live records to validate the connection before saving. Non-secret settings are stored in SQLite, while the password is stored only in macOS Keychain. The tested client becomes active immediately without restarting.

In step 2, create the first portal administrator. Setup is deliberately unavailable through the Mac's LAN address so another network user cannot claim the installation. The resulting Argon2id portal-password hash—not the password—is stored in SQLite.

The hidden terminal prompt is also available:

```bash
cargo run -- portal-admin --email you@example.com --name "Your Name"
```

The browser form signs in the new administrator immediately. If the terminal command was used, open the portal and sign in. Additional users are created from **Administration → Users & access**. Administrators can later change the live connection from the host Mac under **Administration → Metasys connection**.

## 2. Create the site hierarchy

In **Administration → Buildings & floors**:

1. Add each building.
2. Add its floors and set their display order.
3. Use names that make scope assignments unambiguous.

The portal hides unassigned buildings and floors in both the interface and API responses.

## 3. Upload and simplify drawings

In **Administration → Floor-plan editor**, upload:

- one building-overview PDF for the portal home page; and
- one PDF for each floor that will contain service regions.

The upload limit is 25 MB. The app renders the first page locally with the native macOS PDF renderer, reduces it to at most 1,800 pixels on either axis, and detects dark horizontal and vertical linework. It classifies candidate lines as walls, doors, cubicle partitions, or furniture based on length and line weight. No drawing or extracted image leaves the Mac.

Automatic tracing is intentionally a starting point. Review it in the editor:

- toggle **Show original drawing** to compare the source and clean trace;
- select a line and choose a category to reclassify it;
- drag endpoints to correct geometry;
- add missing linework or delete unwanted features; and
- select **Save traced drawing** when complete.

Text, title blocks, dimensions, and other source detail remain faintly available only when the original-drawing toggle is on. The normal building and floor views display the clean trace.

## 4. Draw service regions and map temperature points

Select a floor plan, choose **Region**, click at least three boundary points, and finish the boundary. Give the region:

- a user-facing name;
- a highlight color;
- the serving FAV box name;
- its Metasys object/reference; and
- the temperature attribute.

For the legacy Metasys UI connector, enter the fully qualified point reference and use attribute `85` for Present Value unless the site uses another attribute. A reference that already ends in `,<attribute>` is accepted as-is. For the public REST connector, enter the object UUID and use `presentValue` (or the site-specific attribute identifier).

The app caches each successful or failed read for 30 seconds to limit server load. Failed reads are shown as unavailable; it never substitutes a generated temperature. A mapping can be checked from Terminal before saving it:

```bash
cargo run -- check-temperature --reference 'YOUR_POINT_REFERENCE' --attribute 85
```

## 5. Create accounts and assign access

| Role | Building access | Service requests | Administration |
|---|---|---|---|
| Admin | All buildings and floors | Create, note, and change status | Full |
| View only | Assigned floors and/or regions | View only | None |
| Operator | All buildings and floors | Add notes and change status | None |
| Reporting staff | Assigned floors and/or regions | Create and view | None |

A floor assignment exposes all named regions on that floor. A region assignment exposes only that region and its containing floor. Operators deliberately cannot create occupant reports; their role is to triage and document requests. Administrators can do both.

For each scoped account, select either whole floors or individual regions. Authorization is checked again by every floor-plan, temperature, and service-request endpoint; hiding a navigation item is not the security boundary.

## 6. Submit and work service requests

Reporting staff or administrators select a highlighted area and choose **Report an issue**. The form requires a contact email and issue type. Supported types are too hot, too cold, lighting, water leak, noise, broken toilet, air quality, and other. Optional detail is stored with the request.

Operators and administrators can add notes and move a request through open, in progress, resolved, and closed states. Scoped users only receive requests associated with their assigned areas.

## Network security

The built-in server supports local and LAN HTTP. Authentication prevents unauthorized application access, but HTTP itself does not encrypt passwords, cookies, floor plans, or service-request contents on the wire. For anything beyond a trusted, access-controlled building network:

- bind the Rust service to `127.0.0.1`;
- place a trusted HTTPS reverse proxy in front of it; and
- have the proxy set `X-Forwarded-Proto: https` so the app marks session cookies `Secure`.

Do not expose this service directly to the public internet. Back up `dashboard.sqlite3`; it contains portal accounts, plans, regions, scopes, and service-request history. Metasys, SMTP, and SQL passwords remain in macOS Keychain rather than SQLite.
