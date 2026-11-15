FROM alpine:3.20

RUN apk add --no-cache openssh-server \
    && adduser -D -s /bin/sh shellhopper \
    && mkdir -p /home/shellhopper/.ssh /run/sshd \
    && chown -R shellhopper:shellhopper /home/shellhopper/.ssh \
    && chmod 700 /home/shellhopper/.ssh

COPY sshd_config /etc/ssh/sshd_config
COPY entrypoint.sh /usr/local/bin/shellhopper-sshd

RUN chmod 755 /usr/local/bin/shellhopper-sshd

EXPOSE 22

ENTRYPOINT ["/usr/local/bin/shellhopper-sshd"]
