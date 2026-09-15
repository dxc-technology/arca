#!/bin/sh
# Inject ARCA_ENDPOINT env var into index.html at container startup.
# If ARCA_ENDPOINT is set, replaces the config placeholder so the login
# screen hides the endpoint field. The placeholder "ARCA_INJECT_ENDPOINT_HERE"
# only appears in the config assignment; the JS detection uses startsWith
# to avoid sed replacing both.

if [ -n "$ARCA_ENDPOINT" ]; then
    sed -i "s|ARCA_INJECT_ENDPOINT_HERE|${ARCA_ENDPOINT}|" \
        /usr/share/nginx/html/index.html
fi

# CA material must not be picked as the server certificate. Match a "ca" *word*
# in the file name, never a bare substring: `arca-server.crt` — what
# `arca tls generate` writes — contains "ca" inside "arca", and a `*ca*` glob
# would skip the very file we are looking for.
is_ca_file() {
    case "${1##*/}" in
        ca.*|ca-*|ca_*|*-ca.*|*_ca.*|*-ca-*|*-ca_*|*_ca-*|*_ca_*) return 0 ;;
        *) return 1 ;;
    esac
}

# If TLS cert dir is mounted, find cert and key and enable HTTPS.
if [ -d /etc/nginx/certs ]; then
    cert=""
    key=""
    unreadable_key=""
    for f in /etc/nginx/certs/*.pem /etc/nginx/certs/*.crt; do
        [ -f "$f" ] || continue
        is_ca_file "$f" && continue
        if [ -z "$cert" ] && grep -q "CERTIFICATE" "$f" 2>/dev/null; then
            # Pick the first certificate file (prefer fullchain/server cert over CA)
            cert="$f"
        fi
    done
    for f in /etc/nginx/certs/*.pem /etc/nginx/certs/*.key; do
        [ -f "$f" ] || continue
        is_ca_file "$f" && continue
        # A private key is mode 0640 and owned by Arca's service group, so this
        # container reads it only through a supplementary group (`group_add` in
        # Compose, `fsGroup` in Kubernetes). Without it the file is there but
        # unreadable — remember that, so the failure is reported instead of
        # silently degrading to plain HTTP.
        if [ ! -r "$f" ]; then
            unreadable_key="$f"
            continue
        fi
        if [ -z "$key" ] && grep -q "PRIVATE KEY" "$f" 2>/dev/null; then
            key="$f"
        fi
    done
    if [ -n "$cert" ] && [ -n "$key" ]; then
        sed "s|/etc/nginx/certs/cert.pem|${cert}|;s|/etc/nginx/certs/key.pem|${key}|" \
            /etc/nginx/nginx-tls.conf > /etc/nginx/http.d/default.conf
        echo "Console TLS enabled: cert=$cert key=$key"
    elif [ -n "$unreadable_key" ]; then
        echo "WARNING: $unreadable_key is not readable by this container" >&2
        echo "WARNING: (running as uid $(id -u), groups $(id -G)) — serving plain HTTP." >&2
        echo "WARNING: Add Arca's service GID 65532 as a supplementary group:" >&2
        echo "WARNING:   compose: group_add: [\"65532\"]   k8s: fsGroup: 65532" >&2
    fi
fi

exec nginx -g 'daemon off;'
