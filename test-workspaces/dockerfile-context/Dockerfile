FROM alpine:3
# The wrong Dockerfile: it sits at the workspace root (the build context). devc would build this
# by mistake if it resolved `build.dockerfile` relative to the context instead of devcontainer.json.
RUN echo "wrong-dockerfile-at-context-root" > /which-dockerfile.txt
