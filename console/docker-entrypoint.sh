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

exec nginx -g 'daemon off;'
