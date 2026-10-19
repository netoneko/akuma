# /usr/local/bin holds the box's own tools (crush, crush-kimi, crush-glm, goose-*, claude-*).
# sshd and the kernel give a session PATH=/sbin:/usr/sbin:/bin:/usr/bin, so none of them
# resolved by name in an interactive shell (herdr panes included). Appended, not prepended:
# /bin stays authoritative.
case ":$PATH:" in
    *:/usr/local/bin:*) ;;
    *) PATH="$PATH:/usr/local/bin"; export PATH ;;
esac
