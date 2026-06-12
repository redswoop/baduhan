#!/usr/bin/env bash
# Raw keystroke echo: prints the hex bytes the terminal sends for each key.
# 'q' quits. Compare output across terminals for the same physical input.
export LC_ALL=C
echo "keyecho: press keys, q quits"
stty raw -echo
while IFS= read -rsn1 c; do
  if [ -z "$c" ]; then
    printf '00 '
    continue
  fi
  printf '%02x ' "'$c"
  [ "$c" = "q" ] && break
done
stty sane
echo
echo "done"
