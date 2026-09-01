# The egress proxy for the authoring eval's enforced-isolation mode (#1058).
#
# The subject sits on a Docker `--internal` network so it cannot read the internet - which is the
# point, because an agent that can read nuthatch's documentation online scores well without the
# builder skill having taught it anything, and RFC-0017's whole premise is that nuthatch "is days
# old and in nobody's training data".
#
# But `claude` has to reach its own model service, and review of #1058 caught that it could not:
# `curl https://api.anthropic.com` inside that network answers *Could not resolve host*. The enforced
# mode was unrunnable and would have failed every subject for the harness's reasons.
#
# So: one host, and only one. This container straddles the internal network and a normal one, and
# tinyproxy's filter denies everything that is not the model API.
FROM alpine:3.20

RUN apk add --no-cache tinyproxy >/dev/null

# `FilterDefaultDeny yes` is what makes this an allow-list rather than a suggestion: anything absent
# from the filter is refused, so adding a host is deliberate and forgetting one fails closed.
RUN printf '%s\n' \
      'Port 8888' \
      'Listen 0.0.0.0' \
      'Timeout 600' \
      'Allow 0.0.0.0/0' \
      'ConnectPort 443' \
      'Filter "/etc/tinyproxy/allow.txt"' \
      'FilterDefaultDeny Yes' \
      'FilterURLs Off' \
      'FilterExtended On' \
      'LogLevel Info' \
    > /etc/tinyproxy/tinyproxy.conf \
 && printf '%s\n' '^api\.anthropic\.com$' > /etc/tinyproxy/allow.txt

EXPOSE 8888
CMD ["tinyproxy", "-d", "-c", "/etc/tinyproxy/tinyproxy.conf"]
