#!/bin/bash
# ttyd wrapper for the browser demo: fresh interactive zsh using demo/.zshrc.
export ZDOTDIR=/home/user/fore/demo
export PATH="$HOME/.local/bin:$PATH"
exec zsh -i
