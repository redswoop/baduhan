#!/usr/bin/env bash
# Terminal rendering test card. Every row puts one glyph per cell between
# pipes; if a glyph's advance is wrong, the pipes drift off the ruler.
# Run in any terminal and compare screenshots.
printf 'ruler  |0|1|2|3|4|5|6|7|8|9|\n'
printf 'ascii  |a|b|c|d|e|f|g|h|i|j|\n'
printf 'brail  |⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏|\n'
# NF BMP-PUA icons (verify bytes with `od -c` after editing — PUA chars
# don't survive every editor): e0a0 branch, e0b0/e0b2 powerline, f121
# code, e718 node, f09b github
printf 'nf-bmp |||||||\n'
printf 'nf-md  |󰋜|󰌠|󰊢|󰈙|󰉋|\n'
printf 'box    |┌|┬|┐|├|┼|┤|░|▒|▓|█|\n'
printf 'ruler2 |--|--|--|--|--|\n'
printf 'wide   |日|本|語|中|한|\n'
printf 'emoji  |😀|🚀|⭐|🍣|🔥|\n'
printf 'comb   |é|é|ñ|ą|ü|  (2nd é is e+U+0301)\n'
printf 'style  \033[1mbold\033[0m \033[3mitalic\033[0m \033[4munder\033[0m \033[9mstrike\033[0m \033[4:3m\033[58:2::255:80:80mcurl\033[0m \033[7minverse\033[0m\n'
printf 'color  \033[31m█\033[33m█\033[32m█\033[36m█\033[34m█\033[35m█\033[0m 256:\033[38;5;196m█\033[38;5;46m█\033[38;5;21m█\033[0m true:'
for i in 0 1 2 3 4 5 6 7 8 9; do printf '\033[38;2;%d;%d;128m█' $((255 - i * 25)) $((i * 25)); done
printf '\033[0m\n'
printf 'spin   '
for f in ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏; do printf '\r\033[7Cspin %s after-text-should-not-move |' "$f"; sleep 0.15; done
printf '\n'
