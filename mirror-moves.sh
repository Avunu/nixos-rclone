# shellcheck shell=bash
#
# mirror_moves <src_dir> <src_ext> <dst_dir> <dst_ext>
#
# markdownSync projects one tree onto the other *by path*, so a file moved or
# renamed on one side reads as "deleted here, created there" on the other. The
# stale counterpart then regenerates the document at its old path on the next
# run, so the move never sticks and the file ends up at both paths, in both
# trees, permanently. rclone bisync is no help: it has no rename tracking and
# models every move as delete + create.
#
# Pair the orphaned destination with the newly-appeared source and move it, so
# the relocation is followed instead of duplicated.
#
# Requires: shopt -s globstar nullglob
mirror_moves() {
  local src_dir="$1" src_ext="$2" dst_dir="$3" dst_ext="$4"
  local -A src_stems=() dst_stems=()
  local -a orphans=() fresh=()
  local f rel stem

  for f in "$src_dir"/**/*"$src_ext"; do
    rel="${f#"$src_dir"/}"
    src_stems["${rel%"$src_ext"}"]=1
  done
  for f in "$dst_dir"/**/*"$dst_ext"; do
    rel="${f#"$dst_dir"/}"
    dst_stems["${rel%"$dst_ext"}"]=1
  done

  # Destination files whose source counterpart vanished, and source files with
  # no destination counterpart yet. A move shows up as exactly one of each.
  for stem in "${!dst_stems[@]}"; do
    [ -n "${src_stems[$stem]:-}" ] || orphans+=("$stem")
  done
  for stem in "${!src_stems[@]}"; do
    [ -n "${dst_stems[$stem]:-}" ] || fresh+=("$stem")
  done
  # Nothing to pair. Notably, an empty source tree (unmounted vault) yields no
  # new stems, so a missing side can never trigger a wave of moves.
  { [ ${#orphans[@]} -gt 0 ] && [ ${#fresh[@]} -gt 0 ]; } || return 0

  # Two pairing passes, in this order:
  #   basename - survives a relocation, even if the file was edited in transit
  #   mtime    - survives a rename, which changes the basename but not the
  #              timestamp (a Drive move rewrites parents, not modifiedTime,
  #              and touch -r has already copied that onto the counterpart)
  # Only unambiguous 1:1 matches are acted on, in either pass.
  local -A o_n o_s f_n f_s
  local mode key i oi fi from to
  for mode in basename mtime; do
    o_n=(); o_s=(); f_n=(); f_s=()
    for i in "${!orphans[@]}"; do
      stem="${orphans[$i]}"
      [ -n "$stem" ] || continue
      if [ "$mode" = basename ]; then
        key="${stem##*/}"
      else
        key=$(stat -c '%.9Y' "$dst_dir/$stem$dst_ext")
      fi
      o_n[$key]=$(( ${o_n[$key]:-0} + 1 ))
      o_s[$key]="$i"
    done
    for i in "${!fresh[@]}"; do
      stem="${fresh[$i]}"
      [ -n "$stem" ] || continue
      if [ "$mode" = basename ]; then
        key="${stem##*/}"
      else
        key=$(stat -c '%.9Y' "$src_dir/$stem$src_ext")
      fi
      f_n[$key]=$(( ${f_n[$key]:-0} + 1 ))
      f_s[$key]="$i"
    done

    for key in "${!o_n[@]}"; do
      [ "${o_n[$key]}" -eq 1 ] && [ "${f_n[$key]:-0}" -eq 1 ] || continue
      oi="${o_s[$key]}"
      fi="${f_s[$key]}"
      from="$dst_dir/${orphans[$oi]}$dst_ext"
      to="$dst_dir/${fresh[$fi]}$dst_ext"
      # Never clobber: if something already sits at the new path, this is not
      # the simple relocation it looks like.
      if [ -e "$to" ]; then
        echo "markdown-sync: not moving '$from', '$to' already exists" >&2
        continue
      fi
      mkdir -p "$(dirname "$to")"
      mv "$from" "$to"
      echo "markdown-sync: followed move by $mode: ${orphans[$oi]}$dst_ext -> ${fresh[$fi]}$dst_ext" >&2
      orphans[$oi]=""
      fresh[$fi]=""
    done
  done

  # Whatever is left is a genuine create/delete, or a file that was renamed
  # *and* edited in the same window. Say so rather than silently duplicating.
  for stem in "${orphans[@]}"; do
    [ -n "$stem" ] && echo "markdown-sync: unpaired orphan '$stem$dst_ext'" >&2
  done
  for stem in "${fresh[@]}"; do
    [ -n "$stem" ] && echo "markdown-sync: unpaired new file '$stem$src_ext'" >&2
  done
  return 0
}
