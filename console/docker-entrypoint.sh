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

# If TLS cert dir is mounted, find cert and key and enable HTTPS.
if [ -d /etc/nginx/certs ]; then
    cert=""
    key=""
    for f in /etc/nginx/certs/*.pem /etc/nginx/certs/*.crt; do
        [ -f "$f" ] || continue
        if grep -q "CERTIFICATE" "$f" 2>/dev/null && [ -z "$cert" ]; then
            # Pick the first certificate file (prefer fullchain/server cert over CA)
            case "$f" in
                *ca*) ;;  # skip CA certs
                *) cert="$f" ;;
            esac
        fi
    done
    for f in /etc/nginx/certs/*.pem /etc/nginx/certs/*.key; do
        [ -f "$f" ] || continue
        if grep -q "PRIVATE KEY" "$f" 2>/dev/null && [ -z "$key" ]; then
            case "$f" in
                *ca*) ;;  # skip CA keys
                *) key="$f" ;;
            esac
        fi
    done
    if [ -n "$cert" ] && [ -n "$key" ]; then
        sed "s|/etc/nginx/certs/cert.pem|${cert}|;s|/etc/nginx/certs/key.pem|${key}|" \
            /etc/nginx/nginx-tls.conf > /etc/nginx/http.d/default.conf
        echo "Console TLS enabled: cert=$cert key=$key"
    fi
fi

exec nginx -g 'daemon off;'
