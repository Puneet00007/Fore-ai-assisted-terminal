# fore — zsh plugin
# Usage: eval "$(fore init zsh)"   (the binary prepends a few typeset lines from your config)
#
# This file is intentionally dumb. Its jobs:
#   1. Tell the daemon when a command starts (preexec) and finishes (precmd).
#   2. Capture the tail of stderr so failures can be diagnosed.
#   3. Ask the daemon for a suggestion after each keystroke; paint it as ghost text.
#   4. Keybindings:  →/End/Ctrl-E accept ghost text
#                    Ctrl-/           fix the last failed command
#                    Ctrl-Space       English → command
#   5. Pre-flight on Enter (guards / previews / insights).
#   6. Route `rm` through the trash so `fore undo` works.
#
# No external tools are used (no jq, no python): the binary speaks a TAB-separated
# line format (`--shell`) that plain zsh can split.
# Everything else — ranking, learning, safety, AI — is the daemon's problem.

# ---------------------------------------------------------------------------
# 0. Guard rails
# ---------------------------------------------------------------------------
(( ${+commands[fore]} )) || { print -u2 "fore: binary not in PATH"; return 1; }
[[ -o interactive ]] || return 0
[[ -n $FORE_LOADED ]] && return 0          # sourced twice (e.g. .zshrc re-sourced)
typeset -g FORE_LOADED=1
export FORE_PLUGIN=1      # lets `fore doctor` see that this shell has the plugin

autoload -Uz add-zsh-hook
# $EPOCHREALTIME is empty until this module is loaded. Without it every hook
# below dies with an arithmetic error — silently. Ask me how I know.
zmodload zsh/datetime

typeset -g FORE_SESSION="${FORE_SESSION:-$$-$EPOCHREALTIME}"
typeset -g _fore_cmd_start=0
: ${FORE_GHOST_STYLE:=fg=8} ${FORE_PREFLIGHT:=1} ${FORE_SAFE_RM:=1} ${FORE_BIND_RIGHT:=1}

# stderr capture: 1 = on (default). Set FORE_CAPTURE_STDERR=0 to disable.
typeset -g FORE_CAPTURE_STDERR="${FORE_CAPTURE_STDERR:-1}"
typeset -g _fore_err_file=""
typeset -g _fore_err_dir="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/fore-$UID"
mkdir -p -m 700 "$_fore_err_dir" 2>/dev/null

# Auto-start the daemon if nothing answers (first shell after boot, no service manager,
# daemon crashed…). `fore start` returns immediately when one is already running.
# Runs in the background so shell startup stays at ~3 ms.
if [[ -z $FORE_NO_AUTOSTART ]]; then
  { fore status &>/dev/null || fore start &>/dev/null } &!
fi

# ---------------------------------------------------------------------------
# 1. Observe: command start / command end
# ---------------------------------------------------------------------------
_fore_git_branch() {
  local dir=$PWD
  while [[ $dir != / && -n $dir ]]; do
    if [[ -f $dir/.git/HEAD ]]; then
      local head; read -r head < "$dir/.git/HEAD"
      print -r -- "${head#ref: refs/heads/}"
      return
    fi
    dir=${dir:h}
  done
}

# --- stderr capture -----------------------------------------------------------
# How: duplicate the terminal's stderr to fd 9, then point fd 2 at a process that
# tees to BOTH the real terminal and a temp file. The user sees stderr exactly as
# before; we get a copy. After the command, restore fd 2 from fd 9.
# Full-screen / interactive programs are skipped (they check isatty(2)).
_fore_is_interactive() {
  local first=${1%% *}
  first=${first##*/}
  case $first in
    vim|vi|nvim|nano|emacs|less|more|man|top|htop|btop|ssh|mosh|tmux|screen|fzf|sudo|su|\
    python|python3|ipython|node|irb|psql|mysql|sqlite3|docker|kubectl|gdb|lldb|watch|\
    tig|lazygit|ranger|nnn|mc|vifm|micro|helix|hx|claude|codex|aider|fore) return 0 ;;
  esac
  # anything that already redirects stderr, or uses pipes/heredocs, is left alone
  [[ $1 == *'2>'* || $1 == *'&>'* || $1 == *'<<'* ]] && return 0
  return 1
}

_fore_preexec() {
  _fore_cmd_start=$EPOCHREALTIME
  # Leading space = "don't record" (same convention as HIST_IGNORE_SPACE). The daemon
  # enforces it too, but not sending is cheaper and keeps it out of the socket entirely.
  if [[ $1 != ' '* ]]; then
    local -a extra
    local branch=$(_fore_git_branch)
    [[ -n $branch ]] && extra=(--git-branch "$branch")
    fore exec --session "$FORE_SESSION" --cwd "$PWD" "${extra[@]}" -- "$1" &>/dev/null &!
  fi

  _fore_err_file=""
  if [[ $FORE_CAPTURE_STDERR == 1 ]] && ! _fore_is_interactive "$1"; then
    _fore_err_file="$_fore_err_dir/err.$$.$RANDOM"
    exec 9>&2                                   # save the real stderr
    exec 2> >(tee -a "$_fore_err_file" >&9)     # fd2 → tee → (file, real stderr)
  fi
}

_fore_precmd() {
  local code=$?             # MUST be the very first thing: $? is fragile.
  if [[ -n $_fore_err_file ]]; then
    exec 2>&9 9>&-          # restore stderr, close the saved copy
    sleep 0.02              # let tee flush the last bytes
  fi
  (( _fore_cmd_start == 0 )) && return   # first prompt of the session
  local dur_ms=$(( (EPOCHREALTIME - _fore_cmd_start) * 1000 ))
  _fore_cmd_start=0
  local -a extra
  [[ -n $_fore_err_file && -s $_fore_err_file ]] && extra=(--stderr-file "$_fore_err_file")
  [[ -n $_fore_err_file && ! -s $_fore_err_file ]] && command rm -f "$_fore_err_file"
  fore done --session "$FORE_SESSION" --exit-code "$code" --duration-ms "${dur_ms%.*}" "${extra[@]}" &>/dev/null &!
  _fore_err_file=""
}

add-zsh-hook preexec _fore_preexec
add-zsh-hook precmd  _fore_precmd

# ---------------------------------------------------------------------------
# 2. Suggest: paint ghost text after each keystroke
# ---------------------------------------------------------------------------
typeset -g _fore_suggestion=""

_fore_clear_ghost() {
  _fore_suggestion=""
  POSTDISPLAY=""
  region_highlight=("${(@)region_highlight:#*fore*}")
}

_fore_fetch() {
  _fore_clear_ghost
  [[ -z $BUFFER ]] && return 0
  local s
  s=$(fore suggest --session "$FORE_SESSION" --cwd "$PWD" -- "$BUFFER" 2>/dev/null)
  [[ -z $s || $s == "$BUFFER" || $s != "$BUFFER"* ]] && return 0
  _fore_suggestion=$s
  POSTDISPLAY=${s#"$BUFFER"}
  region_highlight+=("${#BUFFER} $(( ${#BUFFER} + ${#POSTDISPLAY} )) $FORE_GHOST_STYLE memo=fore")
}

# --- Coexistence with zsh-autosuggestions -------------------------------------
# If zsh-autosuggestions is loaded (before OR after us), we don't fight over
# POSTDISPLAY. Instead we register as its first strategy, so its widgets, accept
# keys and styling apply, and fore's history-aware ranking supplies the text.
typeset -g FORE_GHOST_MODE=native _fore_last_sugg="" _fore_last_prefix=""

_zsh_autosuggest_strategy_fore() {
  local s
  s=$(fore suggest --session "$FORE_SESSION" --cwd "$PWD" -- "$1" 2>/dev/null)
  if [[ -n $s && $s == "$1"* && $s != "$1" ]]; then
    typeset -g suggestion=$s
    _fore_last_sugg=$s; _fore_last_prefix=$1
  fi
}

_fore_enable_autosuggestions_mode() {
  FORE_GHOST_MODE=autosuggestions
  ZSH_AUTOSUGGEST_STRATEGY=(fore ${ZSH_AUTOSUGGEST_STRATEGY:#fore})
  # Hand the accept keys back to the standard widgets (zsh-autosuggestions wraps those).
  if [[ ${widgets[_fore_accept]} == user:* ]]; then
    [[ $FORE_BIND_RIGHT == 1 ]] && bindkey '^[[C' forward-char && bindkey '^[OC' forward-char
    bindkey '^[[F' end-of-line; bindkey '^[OF' end-of-line; bindkey '^[[4~' end-of-line; bindkey '^E' end-of-line
  fi
  _fore_clear_ghost 2>/dev/null
}

# Native ghost text (used unless zsh-autosuggestions shows up).
# Chain, don't clobber: if another plugin already redefined self-insert
# (syntax-highlighting, vi-mode, etc.), call THAT instead of the builtin.
_fore_wrap_widget() {   # $1 = widget name
  local prev=${widgets[$1]}
  prev=${prev#user:}
  if [[ ${widgets[$1]} == user:* && $prev != _fore_* ]]; then
    zle -N "_fore_orig_$1" "$prev"
    eval "_fore_w_$1() { zle _fore_orig_$1 -- \"\$@\"; _fore_confirmed=''; [[ \$FORE_GHOST_MODE == native ]] && _fore_fetch; return 0; }"
  else
    eval "_fore_w_$1() { zle .$1 -- \"\$@\"; _fore_confirmed=''; [[ \$FORE_GHOST_MODE == native ]] && _fore_fetch; return 0; }"
  fi
  zle -N "$1" "_fore_w_$1"
}

# Accept: → / End / Ctrl-E. → only when the cursor is at the end, so it stays
# a normal cursor key mid-line.
_fore_accept() {
  if [[ -n $_fore_suggestion && $CURSOR -eq ${#BUFFER} ]]; then
    local saved=$(( ${#_fore_suggestion} - ${#BUFFER} ))
    BUFFER=$_fore_suggestion
    CURSOR=${#BUFFER}
    _fore_clear_ghost
    fore accepted --session "$FORE_SESSION" --chars "$saved" &>/dev/null &!
  else
    case $KEYS in
      $'\e[F'|$'\eOF'|$'\e[4~'|$'\C-e') zle .end-of-line ;;
      *) zle .forward-char ;;
    esac
  fi
}

if (( ${+functions[_zsh_autosuggest_start]} )); then
  _fore_enable_autosuggestions_mode
else
  _fore_wrap_widget self-insert
  _fore_wrap_widget backward-delete-char
  _fore_wrap_widget delete-char
  _fore_wrap_widget backward-kill-word
  zle -N _fore_accept
  if [[ $FORE_BIND_RIGHT == 1 ]]; then
    bindkey '^[[C' _fore_accept   # →
    bindkey '^[OC' _fore_accept   # → (application mode)
  fi
  bindkey '^[[F' _fore_accept     # End
  bindkey '^[OF' _fore_accept
  bindkey '^[[4~' _fore_accept
  bindkey '^E'   _fore_accept     # Ctrl-E

  # Clear ghost text when the line is submitted or abandoned; chain any existing hook.
  if [[ ${widgets[zle-line-finish]} == user:* ]]; then
    zle -N _fore_orig_line_finish "${widgets[zle-line-finish]#user:}"
    _fore_line_finish() { POSTDISPLAY=""; _fore_suggestion=""; zle _fore_orig_line_finish -- "$@"; }
  else
    _fore_line_finish() { POSTDISPLAY=""; _fore_suggestion=""; }
  fi
  zle -N zle-line-finish _fore_line_finish
fi

# Runs once, at the first prompt — i.e. after ALL of .zshrc. Catches
# zsh-autosuggestions sourced after us, and re-asserts our strategy if the
# user's .zshrc overwrote ZSH_AUTOSUGGEST_STRATEGY later.
_fore_late_init() {
  add-zsh-hook -d precmd _fore_late_init
  if (( ${+functions[_zsh_autosuggest_start]} )); then
    _fore_enable_autosuggestions_mode
  fi
}
add-zsh-hook precmd _fore_late_init

# ---------------------------------------------------------------------------
# 3. AI: Ctrl-/ fix, Ctrl-Space ask
# ---------------------------------------------------------------------------
# Both put the proposed command INTO THE LINE EDITOR, never run it. You read the
# badge, then press Enter — or edit it, or Ctrl-C. Destructive proposals are shown
# in red and additionally prefixed with a `# ` comment so a reflexive Enter is a no-op.

_fore_show_proposal() {
  # $1 = output of `fore fix --shell` / `fore ask --shell`: lines of KEY<TAB>value
  local out=$1 line key val cmd note risk why src
  for line in ${(f)out}; do
    key=${line%%$'\t'*}; val=${line#*$'\t'}
    case $key in
      CMD)  cmd=$val ;;  NOTE) note=$val ;;  RISK) risk=$val ;;
      WHY)  why=$val ;;  SRC)  src=$val ;;
      ERR)  zle -M "fore: $val"; return 1 ;;
    esac
  done
  [[ -z $cmd ]] && { zle -M "fore: no proposal"; return 1; }

  # zle -M shows plain text only (escape codes come out as ^[[32m), so the badge is
  # text; colour is conveyed by the # comment convention for DESTRUCTIVE below.
  local badge
  case $risk in
    read-only)   badge="✔ read-only" ;;
    mutating)    badge="~ mutating" ;;
    remote)      badge="⇅ remote" ;;
    privileged)  badge="# privileged" ;;
    DESTRUCTIVE) badge="✖ DESTRUCTIVE" ;;
    *)           badge="[$risk]" ;;
  esac
  local msg="[$badge]  $why   ($src)"
  [[ -n $note ]] && msg="$note"$'\n'"$msg"
  zle -M "$msg"

  if [[ $risk == DESTRUCTIVE ]]; then
    BUFFER="# $cmd"       # user must consciously delete the `# ` to run it
  else
    BUFFER=$cmd
  fi
  CURSOR=${#BUFFER}
  _fore_clear_ghost
}

_fore_fix() {
  zle -M "fore: thinking…"
  zle -R
  local out
  out=$(fore fix --session "$FORE_SESSION" --shell 2>&1)
  _fore_show_proposal "$out"
}
zle -N _fore_fix
bindkey '^_' _fore_fix        # Ctrl-/ sends ^_ in most terminals
bindkey '^[f' _fore_fix       # Alt-f as a fallback

_fore_ask() {
  local req=$BUFFER
  if [[ -z ${req//[[:space:]]/} ]]; then
    zle -M "fore: type what you want in English, then press Ctrl-Space"
    return
  fi
  zle -M "fore: translating…"
  zle -R
  local -a extra
  local branch=$(_fore_git_branch)
  [[ -n $branch ]] && extra=(--git-branch "$branch")
  local out
  out=$(fore ask --session "$FORE_SESSION" --cwd "$PWD" "${extra[@]}" --shell -- "$req" 2>&1)
  _fore_show_proposal "$out"
}
zle -N _fore_ask
bindkey '^@' _fore_ask        # Ctrl-Space sends ^@ (NUL)
bindkey '^[a' _fore_ask       # Alt-a as a fallback

# ---------------------------------------------------------------------------
# 4. Pre-flight on Enter
# ---------------------------------------------------------------------------
#   info / warn     → print the notes above the prompt, then run. Never blocks.
#   block           → print in red, DON'T run, and remember the exact buffer.
#                     Pressing Enter again on the same unchanged line runs it.
#                     Any edit resets the confirmation.
# The daemon is given ≤400 ms; if it's slow or down, Enter behaves normally.
# Disable per-shell with FORE_PREFLIGHT=0. Silence checks with FORE_IGNORE=venv,prod.

typeset -g _fore_confirmed=""

_fore_git_dirty() {
  local out
  out=$(command git status --porcelain 2>/dev/null | head -1)
  [[ -n $out ]] && print 1 || print 0
}

_fore_accept_line() {
  _fore_clear_ghost
  local line=$BUFFER
  # In zsh-autosuggestions mode we never see the accept keystroke; infer it.
  if [[ $FORE_GHOST_MODE == autosuggestions && -n $_fore_last_sugg && $line == "$_fore_last_sugg" && ${#_fore_last_prefix} -lt ${#line} ]]; then
    fore accepted --session "$FORE_SESSION" --chars $(( ${#line} - ${#_fore_last_prefix} )) &>/dev/null &!
  fi
  _fore_last_sugg=""
  if [[ $FORE_PREFLIGHT != 1 || -z ${line//[[:space:]]/} ]]; then
    zle _fore_orig_accept_line; return
  fi
  if [[ -n $_fore_confirmed && $_fore_confirmed == "$line" ]]; then
    _fore_confirmed=""
    zle _fore_orig_accept_line; return
  fi
  _fore_confirmed=""

  local branch=$(_fore_git_branch) dirty=""
  [[ $line == git* || $line == *"&& git"* ]] && dirty=$(_fore_git_dirty)

  local out
  out=$(FORE_GIT_BRANCH="$branch" FORE_GIT_DIRTY="$dirty" fore preflight --session "$FORE_SESSION" --cwd "$PWD" --shell -- "$line" 2>/dev/null)
  local -a lines; lines=(${(f)out})
  local verdict=${lines[1]}
  if [[ -z $verdict || ${#lines} -le 1 ]]; then zle _fore_orig_accept_line; return; fi

  # Two renderings: coloured (printed to the terminal before a command runs) and
  # plain (for `zle -M`, which shows escape codes literally).
  local rendered="" plain="" l sev text
  for l in "${lines[@]:1}"; do
    sev=${l%%$'\t'*}; text=${l#*$'\t'}
    case $sev in
      block) rendered+=$'\e[1;31m✖\e[0m '"$text"$'\n'; plain+="✖ $text"$'\n' ;;
      warn)  rendered+=$'\e[33m!\e[0m '"$text"$'\n';    plain+="! $text"$'\n' ;;
      *)     rendered+=$'\e[2m·\e[0m '"$text"$'\n';     plain+="· $text"$'\n' ;;
    esac
  done
  rendered=${rendered%$'\n'}; plain=${plain%$'\n'}

  if [[ $verdict == BLOCK ]]; then
    _fore_confirmed=$line
    zle -M "$plain"$'\n'"↳ press Enter again to run, or edit the line"
    return
  fi
  print -r -- ""
  print -r -- "$rendered"
  zle _fore_orig_accept_line
}
# Chain whatever accept-line was there (e.g. zsh-syntax-highlighting, atuin).
if [[ ${widgets[accept-line]} == user:* && ${widgets[accept-line]#user:} != _fore_* ]]; then
  zle -N _fore_orig_accept_line "${widgets[accept-line]#user:}"
else
  _fore_orig_accept_line() { zle .accept-line -- "$@"; }
  zle -N _fore_orig_accept_line
fi
zle -N accept-line _fore_accept_line

# ---------------------------------------------------------------------------
# 5. Safe rm  (rm → trash; `fore undo` restores; `command rm` / `\rm` bypass)
# ---------------------------------------------------------------------------
if [[ $FORE_SAFE_RM == 1 ]]; then
  alias rm='fore rm'
fi

# ---------------------------------------------------------------------------
# 6. Mined aliases
# ---------------------------------------------------------------------------
typeset -g FORE_ALIAS_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/fore/aliases.zsh"
[[ -r $FORE_ALIAS_FILE ]] && source "$FORE_ALIAS_FILE"
export FORE_EXISTING_ALIASES="${(k)aliases}"
