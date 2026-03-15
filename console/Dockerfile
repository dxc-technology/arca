FROM nginx:alpine

# Remove default nginx config
RUN rm /etc/nginx/conf.d/default.conf

# Copy both HTTP and HTTPS nginx configs
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY nginx-tls.conf /etc/nginx/nginx-tls.conf

# Copy web console files
COPY index.html /usr/share/nginx/html/
COPY logo.svg /usr/share/nginx/html/
COPY favicon.png /usr/share/nginx/html/
COPY favicon-16.png /usr/share/nginx/html/
COPY apple-touch-icon.png /usr/share/nginx/html/
COPY apple-touch-icon-precomposed.png /usr/share/nginx/html/

# Copy entrypoint script
COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /docker-entrypoint.sh

EXPOSE 80 443

ENTRYPOINT ["/docker-entrypoint.sh"]
