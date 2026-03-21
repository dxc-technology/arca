# Web Console

Arca includes a browser-based web console for managing buckets, objects, and access control. The console is a **separate application** that communicates with Arca exclusively via the S3 API and Admin API — it's just another API client.

## Overview

- **Technology**: single-file Alpine.js + Tailwind CSS application, served by nginx:alpine
- **Docker image**: `arca-console`, ~40 MB
- **Port**: 80 (mapped to 9080 on the host by default), 443 for TLS
- **Theme**: "The Vault" — dark glassmorphism design

The console is intentionally not embedded in the Arca binary. This keeps the server binary small (~8.6 MB), allows independent release cycles, and supports split deployment (Arca on a hardened VM, console on a separate host).

## Starting the Console

=== "HTTP"

    ```bash
    bin/console start -d --build
    ```

    This starts the console on [http://localhost:9080](http://localhost:9080).

=== "HTTPS"

    ```bash
    bin/console start -d --build --tls
    ```

    This starts the console on port **9443** with TLS, using certificates from the `certs/` directory. Enter the Arca HTTPS endpoint at the login screen.

!!! tip
    Drop the `-d` flag to run in the foreground and see container logs in real time. Press ++ctrl+c++ to stop.

The `ARCA_ENDPOINT` environment variable controls which Arca server the console connects to. In the default Docker Compose setup, it's set to `http://localhost:9000` so the browser (running on the host) can reach Arca directly. With `--tls`, the endpoint is not preset — enter it at the login screen.

## Login

![Console login screen](../assets/screenshots/console-login.png)

Enter the Arca endpoint URL and your credentials:

- **Endpoint**: the URL of your Arca server (e.g., `http://localhost:9000`)
- **Access Key**: your S3 access key ID
- **Secret Key**: your S3 secret access key

!!! tip
    When `ARCA_ENDPOINT` is set in the Docker environment, the endpoint field is pre-filled and hidden. You only need to enter your credentials.

The console determines your role after connecting. Admin credentials unlock the sidebar sections for **Dashboard**, **Users**, **Teams**, and **Grants**. Non-admin credentials only see the **Buckets** section.

## Dashboard

![Console dashboard](../assets/screenshots/console-dashboard.png)

The dashboard is available to **admin credentials only**. It shows:

- **Server info**: version, uptime
- **Storage stats**: bucket count, object count, total storage size
- **Storage distribution**: SVG donut chart showing size per bucket
- **Encryption status**: whether server-side encryption is active (SSE-S3 with local key or Vault/OpenBAO KMS)
- **Health indicator**: real-time server health status with auto-refresh

## Bucket Management

![Bucket list](../assets/screenshots/console-buckets.png)

The Buckets view shows all buckets as cards with creation date. Encrypted buckets display a green shield badge, and versioned buckets display a blue clock icon (or amber pause icon if versioning is suspended). From here you can:

- **Create** a new bucket (click the "+" button)
- **Delete** an empty bucket
- **Browse** a bucket by clicking its card
- **Open settings** via the gear icon on each card

## Object Browser

![Bucket browser](../assets/screenshots/console-bucket-browser.png)

Inside a bucket, the object browser provides:

- **Breadcrumb navigation** — click any path segment to navigate up. When encryption is active, a green shield icon appears next to the bucket name.
- **Folder browsing** — click folders to navigate into them
- **Create folder** — creates an S3 directory marker (zero-byte object with trailing `/`)
- **Upload Files** — click to select files, or drag-and-drop onto the browser
- **Upload Folder** — upload an entire directory tree preserving its structure
- **Download** — download individual objects
- **Share** — generate a presigned URL for any object (see [Sharing Objects](#sharing-objects) below)
- **Delete** — delete objects or folders (with their contents)

### Object Detail

![Object detail panel](../assets/screenshots/console-object-detail.png)

Click any object to open the detail panel, which shows:

- Key, size, content type
- ETag and last modified timestamp
- Encryption status (when encryption is active)
- **Share**, **Download**, and **Delete** actions

### Sharing Objects

![Share modal](../assets/screenshots/console-share-modal.png)

Click the **Share** button on any object (either inline or in the detail panel) to generate a presigned URL. The share modal lets you:

1. Choose an expiry duration: **1 hour**, **6 hours**, **1 day**, or **7 days**
2. Click **Generate Link** to create the presigned URL
3. **Copy** the URL to share with anyone, no credentials required

The generated URL provides time-limited, read-only access to the object. It expires automatically after the chosen duration.

!!! note
    Presigned URLs are generated via the Admin API (`/admin/presign`). This feature requires admin credentials.

### Batch Operations

![Batch selection](../assets/screenshots/console-batch-selection.png)

Select multiple objects and folders using the checkboxes next to each item. When items are selected, an action bar appears with:

- **Download .tar.gz** — streams a compressed archive of all selected objects
- **Delete** — batch-delete all selected items (with confirmation dialog)
- **Clear selection** — deselect all items

For folder selections, deletion is recursive: all objects under the selected folders are removed.

### Treemap Visualization

![Treemap view](../assets/screenshots/console-treemap.png)

Toggle the treemap view to see a visual representation of object sizes within a bucket. Larger objects appear as larger rectangles, making it easy to spot storage-heavy files.

## Bucket Settings

![Bucket settings](../assets/screenshots/console-bucket-settings.png)

Click the gear icon in the bucket browser header (or on a bucket card) to open the settings view. It contains:

### Encryption

- When **server-level encryption is enabled**, the card shows a read-only status indicator (e.g., "SSE-S3 (AES-256) Active" or "SSE-S3 (Vault) Active").
- When **server-level encryption is disabled** (per-bucket mode), you can toggle encryption on or off for individual buckets. New objects written after enabling encryption are encrypted; existing objects remain unchanged.

### Versioning

![Versioning settings](../assets/screenshots/console-versioning-settings.png)

The versioning card lets you control object versioning for the bucket. The toggle cycles through three states:

| State | Description |
|-------|-------------|
| **Not versioned** | Default. Objects are overwritten in place, no history is kept. |
| **Enabled** (green) | All object writes create new versions. Deletes create delete markers instead of removing data. |
| **Suspended** (amber) | No new versions are created, but existing versions are preserved. New writes use a `null` version ID. |

!!! note
    Versioning cannot be disabled once it has been enabled, only suspended. This matches the S3 specification.

### Danger Zone

- **Delete this bucket** — permanently removes the bucket. Requires typing the bucket name to confirm. The bucket must be empty.

## Object Versioning

When versioning is enabled on a bucket, the console provides several features for managing object versions, viewing deleted objects, and restoring data.

### Versioning Indicators

Versioned buckets are marked with visual indicators throughout the console:

- **Bucket list**: a blue clock icon with "Versioned" label (or amber with "Suspended")
- **Object browser breadcrumb**: a clock icon appears next to the bucket name
- **Dashboard legend**: versioning status is shown next to each bucket in the storage distribution chart

### Viewing Deleted Objects

![Show deleted objects](../assets/screenshots/console-show-deleted.png)

When browsing a versioned bucket, a **Show deleted** toggle appears in the toolbar. Enabling it reveals:

- **Deleted files** — shown with a strikethrough name, red "Deleted" badge, and the deletion date. Click a deleted file to open its detail panel and view its version history.
- **Deleted folders** — shown as faded folder icons with a strikethrough name and "Deleted" badge. These are directories where all contained objects have delete markers as their latest version.

### Version History

![Version history panel](../assets/screenshots/console-version-history.png)

When viewing an object in a versioned bucket, the detail panel includes a collapsible **Versions** section. Click it to expand the version list, which shows:

- **Version ID** — truncated to 8 characters, hover for the full ID
- **Latest** badge (blue) — marks the current version
- **Delete Marker** badge (red) — marks versions that are deletion records, not actual data
- **Timestamp** and **size** for each version
- **Download** button — download a specific version (not available for delete markers)
- **Delete** button — permanently remove a specific version

### Deleting Versions

![Delete version modal](../assets/screenshots/console-delete-version-modal.png)

Click the delete button on any version to open a confirmation modal. The modal behavior varies based on the version type:

- **Regular version**: the modal title reads "Delete Version" and warns that the action is permanent and cannot be undone.
- **Delete marker**: the modal title reads "Remove Delete Marker" and explains that removing it will restore the object.

Both modals show the version ID and object key. A "Latest" badge appears if you are about to delete the current version.

!!! tip
    To **restore a deleted object**, toggle "Show deleted" on, click the deleted file, expand its version history, and remove the delete marker. The most recent non-deleted version becomes the current version.

## Credential Management

![Credentials list](../assets/screenshots/console-credentials.png)

The Credentials view is available to **admin credentials only** (navigate to `#/credentials`). It shows all credentials across all users as cards, each displaying:

- **Active/Inactive** badge — green for active, red for inactive
- **Admin/User** badge — purple for admin privileges, gray for regular user
- **Access Key ID** — the full key ID
- **Description** — what the credential is used for
- **Creation date**

A warning banner appears when only one active credential remains, to prevent accidental lockout.

### Creating Credentials

![Credential created](../assets/screenshots/console-credential-created.png)

Click "+ Create Credential" to open the creation form:

1. Enter an optional **description** (e.g., "CI/CD Pipeline")
2. Check **Admin privileges** if the credential needs access to the Admin API and console management
3. Click **Create**

The secret key is displayed **only once** after creation. Copy it immediately, as it cannot be retrieved later.

## User Management

![Users list](../assets/screenshots/console-users.png)

The Users view is available to **admin credentials only**. It displays all users as cards showing the username, description, credential/team/grant counts, and a "root" badge for the root user.

- **Create user** — click "+ Create User", enter a username and optional description
- **Delete user** — click the trash icon on any non-root user card (users with active credentials must have credentials removed first)
- **Open detail** — click a user card to open the user detail page

### User Detail

![User detail](../assets/screenshots/console-user-detail.png)

The user detail page shows the username as an inline-editable title and the description below it. Both save automatically on change. The root user's name and description are read-only.

Four tabs organize the user's related data:

#### Credentials Tab

The default tab shows all credentials belonging to this user. Each credential card displays:

- **Active/Inactive badge** — click to toggle. Deactivated credentials are rejected at authentication time. Arca prevents deactivating the last active credential or the last active admin credential.
- **Access Key ID**
- **Description** — inline-editable, saves on change

Click "+ Create Credential" to add a new credential. You can optionally provide custom access/secret keys or let Arca auto-generate them. The secret key is displayed only once.

#### Direct Grants Tab

![User grants](../assets/screenshots/console-user-grants.png)

A dual-list shuttle component shows grants attached directly to this user (left) and available grants (right). Assign or remove grants by:

- Double-clicking an item to move it
- Selecting items and clicking the arrow buttons
- Dragging and dropping between lists
- Using the double-arrow buttons to move all items

#### Teams Tab

Same shuttle component for managing team membership. Add the user to teams or remove them.

#### Effective Grants Tab

![User effective grants](../assets/screenshots/console-user-effective.png)

A read-only view showing all grants that apply to this user, combining direct grants and grants inherited through team membership. Each grant card shows the grant name and its source (direct or the team name it comes from).

## Team Management

![Teams list](../assets/screenshots/console-teams.png)

The Teams view is available to **admin credentials only**. It displays all teams as cards showing the team name and description.

- **Create team** — click "+ Create Team", enter a name and optional description
- **Delete team** — click the trash icon on any team card
- **Open detail** — click a team card to open the team detail page

### Team Detail

![Team detail](../assets/screenshots/console-team-detail.png)

The team detail page shows the team name as an inline-editable title and the description below it. Both save automatically on change.

Two tabs organize the team's related data:

#### Members Tab

A dual-list shuttle component showing current team members (left) and available users (right). Manage membership using the same controls as the grants shuttle (double-click, arrows, drag-and-drop).

#### Grants Tab

A dual-list shuttle for managing grants attached to this team. Any grant attached here is inherited by all team members.

## Grant Management

![Grants list](../assets/screenshots/console-grants.png)

The Grants view is available to **admin credentials only**. It displays all grants as cards showing the grant name, description, and a "built-in" badge for system grants.

Three built-in grants are created automatically and cannot be modified or deleted:

| Grant | Description |
|-------|-------------|
| **AdministratorAccess** | Full access to all operations |
| **S3FullAccess** | Full access to all S3 operations |
| **S3ReadOnlyAccess** | Read-only S3 access |

- **Create grant** — click "+ Create Grant", enter a name, description, and policy document (JSON). Template buttons provide common starting policies.
- **Delete grant** — click the trash icon on any non-built-in grant card
- **Open detail** — click a grant card to open the grant detail page

### Grant Detail

![Grant detail](../assets/screenshots/console-grant-detail.png)

The grant detail page shows the grant name as an inline-editable title and the description below it. Both save automatically on change. Built-in grants display read-only name and description.

The detail page has three sections:

#### Policy Document

A JSON text editor for the IAM-style policy document. Template buttons insert common policies (Full Access, S3 Full, S3 Read-Only), and a Format button validates and pretty-prints the JSON. Click "Save Policy" to persist changes.

Built-in grants show the policy document as read-only.

#### Statement Preview

A visual breakdown of the policy statements, color-coded by effect: green for Allow, red for Deny. Each statement shows its actions and resources.

#### Attached To

Shows which users and teams have this grant attached. Click a user or team name to navigate to their detail page.

## Audit Log

![Audit Log](../assets/screenshots/console-audit-log.png)

The Audit Log view is available to **admin credentials only**. It shows a chronological log of all S3 and admin operations performed on the server, with details about who performed them, when, and what the result was.

The table shows: timestamp, operation name, bucket, key, user, HTTP status (color-coded: green for 2xx, amber for 4xx, red for 5xx), and request duration.

### Filtering

The filter bar provides three controls:

- **Bucket** — text input, filters by bucket name (server-side)
- **User ID** — text input, filters by user ID (server-side)
- **Operations** — tag-based filter with autocomplete:
    - Type to search operations, press Enter or click to add as a tag
    - Toggle **IN** (include, show only selected) / **EX** (exclude, hide selected) mode by clicking the mode badge
    - Remove tags with the X button or Backspace
    - Clear all tags with the X button on the right
    - Filters and page size are persisted across navigation

### Detail Panel

Click any row to open a slide-in detail panel on the right showing all fields: operation, timestamp, HTTP method, status, duration, bucket, key, version ID, error code, user ID, access key, source IP, user agent, bytes received/sent, and request ID.

!!! note
    Read-only monitoring operations (Health, Metrics, ListAudit, etc.) are not logged to avoid feedback loops.

## Monitoring

![Monitoring](../assets/screenshots/console-monitoring.png)

The Monitoring view is available to **admin credentials only**. It displays historical metrics as SVG sparkline charts with labeled axes:

- **Objects** — total object count over time
- **Storage Size** — total storage bytes over time
- **Buckets** — bucket count over time
- **Connections** — active HTTP connections at each snapshot

A time range selector at the top right offers: **1h**, **6h**, **24h**, **7d**, **30d**. Each chart shows the current value in the header and the trend line with labeled Y-axis ticks and X-axis time labels.

Metrics snapshots are recorded periodically (default: every 60 seconds) and stored in the database. The retention period is configurable in the Settings page.

## Settings

![Settings](../assets/screenshots/console-settings.png)

The Settings view is available to **admin credentials only**. It manages instance-wide configuration with a TOML > database > default precedence chain:

- Settings defined in the TOML configuration file are **locked** (shown with a lock icon and "Config file" badge, read-only in the console)
- Settings not in the TOML file can be edited from the console (stored in the database, shown with "Custom" or "Default" badge)
- Values save automatically on change

### General

- **S3 Region** — default region for HeadBucket, GetBucketLocation, and presigned URL generation. Can also be set per-bucket.

### Monitoring

- **Audit Log Retention** — days to keep audit log entries (0 = keep forever, default: 90 days)
- **Metrics Retention** — days to keep metrics snapshots (0 = keep forever, default: 30 days)

## Custom Deployment

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ARCA_ENDPOINT` | *(none)* | Arca server URL. When set, pre-fills and hides the endpoint field on the login screen. |

### Separate Network

The console can run on a different host or network from Arca. The browser (not the console container) makes API requests to Arca, so the **browser** must be able to reach the Arca endpoint URL.

```yaml
services:
  console:
    image: arca-console
    ports:
      - "9080:80"
    environment:
      - ARCA_ENDPOINT=https://s3.example.com
```

### Reverse Proxy

When placing the console behind a reverse proxy:

```nginx
server {
    listen 443 ssl;
    server_name console.example.com;

    location / {
        proxy_pass http://console:80;
        proxy_set_header Host $host;
    }
}
```

Since the console is a static single-page application, no special proxy configuration is needed — all API calls go directly from the browser to the Arca endpoint.
