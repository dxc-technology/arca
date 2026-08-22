FROM nginx:alpine

# Remove default nginx config
RUN rm /etc/nginx/conf.d/default.conf

# Copy both HTTP and HTTPS nginx configs
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY nginx-tls.conf /etc/nginx/nginx-tls.conf

# Copy web console files
COPY index.html /usr/share/nginx/html/
COPY js/ /usr/share/nginx/html/js/
COPY assets/ /usr/share/nginx/html/assets/

# Copy entrypoint script
COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /docker-entrypoint.sh

# Run as the image's existing unprivileged `nginx` user (UID 101) instead of
# root. Binding ports 80/443 as non-root needs CAP_NET_BIND_SERVICE on the
# binary itself (Docker grants that capability to containers by default).
# The entrypoint rewrites conf.d/default.conf (TLS mode) and index.html
# (endpoint injection) at startup, so those paths — plus nginx's own temp/
# cache/pid dirs, which default to root-owned /var/run — must be owned by
# that user.
RUN apk add --no-cache libcap && \
    setcap 'cap_net_bind_service=+ep' /usr/sbin/nginx && \
    apk del libcap && \
    sed -i 's|^pid\s\+/run/nginx.pid;|pid /tmp/nginx.pid;|' /etc/nginx/nginx.conf && \
    sed -i '/^user\s\+nginx;/d' /etc/nginx/nginx.conf && \
    chown -R nginx:nginx /etc/nginx/conf.d /usr/share/nginx/html /var/cache/nginx

EXPOSE 80 443

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD wget -q -O- http://localhost:80/ >/dev/null || exit 1

USER nginx

ENTRYPOINT ["/docker-entrypoint.sh"]
