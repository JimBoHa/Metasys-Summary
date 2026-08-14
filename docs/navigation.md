# Application navigation

Authenticated pages share a grouped left sidebar. It stays visible on desktop, becomes a modal drawer below 900 pixels, remembers collapsed groups in browser-local storage, and marks the current destination.

## Groups and roles

| Group | Destinations | Visible to |
|---|---|---|
| Overview | Building overview, service requests | Every signed-in role |
| Operations | Alarm summary, system diagnostics, trend analysis | Administrator, operator |
| Administration | Buildings and floors, floor-plan editor, users, Metasys connection, email reports, SQL source | Administrator |

View-only and reporting-staff users see only Overview navigation. Their existing building/floor/region scopes continue to be checked by the Rust API. Hiding a link is a convenience, not an authorization boundary; every protected page and endpoint still validates the authenticated session and role on the server.

The grouped hierarchy and responsive drawer behavior were informed by [FarmDashboard](https://github.com/mdbro/farm_dashboard). The Metasys markup, styling, role integration, and browser code are an original implementation.
