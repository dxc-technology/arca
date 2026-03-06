# Web Console

Arca includes a browser-based web console for managing buckets, objects, and credentials. The console is a **separate application** that communicates with Arca exclusively via the S3 API and Admin API — it's just another API client.

## Overview

- **Technology**: single-file Alpine.js + Tailwind CSS application, served by nginx:alpine
- **Docker image**: `arca-console`, ~40 MB
- **Port**: 3000 (mapped to 9080 on the host by default)
- **Theme**: "The Vault" — dark glassmorphism design

The console is intentionally not embedded in the Arca binary. This keeps the server binary small (~8.6 MB), allows independent release cycles, and supports split deployment (Arca on a hardened VM, console on a separate host).

## Starting the Console

```bash
bin/console --build -d
```

This starts the console on [http://localhost:9080](http://localhost:9080).

The `ARCA_ENDPOINT` environment variable controls which Arca server the console connects to. In the default Docker Compose setup, it's set to `http://localhost:9000` so the browser (running on the host) can reach Arca directly.

## Login

![Console login screen](../assets/screenshots/console-login.png)

Enter the Arca endpoint URL and your credentials:

- **Endpoint**: the URL of your Arca server (e.g., `http://localhost:9000`)
- **Access Key**: your S3 access key ID
- **Secret Key**: your S3 secret access key

!!! tip
    When `ARCA_ENDPOINT` is set in the Docker environment, the endpoint field is pre-filled and hidden. You only need to enter your credentials.

The console determines your role (admin or user) after connecting. Admin credentials unlock the Dashboard and Credentials sections.

## Dashboard

![Console dashboard](../assets/screenshots/console-dashboard.png)

The dashboard is available to **admin credentials only**. It shows:

- **Server info**: version, uptime
- **Storage stats**: bucket count, object count, total storage size
- **Storage distribution**: SVG donut chart showing size per bucket
- **Health indicator**: real-time server health status with auto-refresh

## Bucket Management

![Bucket list](../assets/screenshots/console-buckets.png)

The Buckets view shows all buckets as cards with object count and total size. From here you can:

- **Create** a new bucket (click the "+" button)
- **Delete** an empty bucket
- **Browse** a bucket by clicking its card

## Object Browser

![Bucket browser](../assets/screenshots/console-bucket-browser.png)

Inside a bucket, the object browser provides:

- **Breadcrumb navigation** — click any path segment to navigate up
- **Folder browsing** — click folders to navigate into them
- **Create folder** — creates an S3 directory marker (zero-byte object with trailing `/`)
- **Upload** — drag-and-drop or click to upload files
- **Download** — download individual objects
- **Delete** — delete objects or folders (with their contents)

### Object Detail

![Object detail panel](../assets/screenshots/console-object-detail.png)

Click any object to open the detail panel, which shows:

- Key, size, content type
- ETag and last modified timestamp
- Download and delete actions

### Treemap Visualization

![Treemap view](../assets/screenshots/console-treemap.png)

Toggle the treemap view to see a visual representation of object sizes within a bucket. Larger objects appear as larger rectangles, making it easy to spot storage-heavy files.

## Credential Management

![Credential list](../assets/screenshots/console-credentials.png)

The Credentials view is available to **admin credentials only**. It displays all credentials as cards showing:

- Access key ID
- Description and role (Admin/User)
- Creation date and status

### Creating Credentials

![Credential created](../assets/screenshots/console-credential-created.png)

Click "Create" to add a new credential:

1. Enter a description (e.g., "CI/CD Pipeline")
2. Optionally check "Admin" for admin privileges
3. Click Create

The secret key is displayed **only once** in a reveal panel. Copy and store it securely before closing.

### Deleting Credentials

Delete a credential by clicking its delete button. A confirmation dialog appears. Arca prevents deleting the last admin credential or the last active credential to avoid lockout.

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
      - "9080:3000"
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
        proxy_pass http://console:3000;
        proxy_set_header Host $host;
    }
}
```

Since the console is a static single-page application, no special proxy configuration is needed — all API calls go directly from the browser to the Arca endpoint.
