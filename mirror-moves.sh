# shellcheck shell=bash
#
# mirror_moves <src_dir> <src_ext> <dst_dir> <dst_ext> [ids_file] [on_move]
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
# ids_file, if given, is a record_ids snapshot of <src_dir> from the previous
# run, and enables pairing by file identity (see below).
#
# on_move, if given, is a command run as `on_move <old_rel> <new_rel>` (paths
# relative to <dst_dir>, with <dst_ext>) *before* each local move, to carry the
# move further, e.g. to the remote. Non-zero skips that pair, leaving it to be
# treated as a delete + create; to abort the run instead, it must exit.
#
# Requires: shopt -s globstar nullglob
mirror_moves() {
  local src_dir="$1" src_ext="$2" dst_dir="$3" dst_ext="$4"
  local ids_file="${5:-}" on_move="${6:-}"
  local -A src_stems=() dst_stems=() prev_ids=()
  local -a orphans=() fresh=()
  local f rel stem id

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

  if [ -n "$ids_file" ] && [ -f "$ids_file" ]; then
    while IFS=$'\t' read -r -d '' id rel; do
      prev_ids["${rel%"$src_ext"}"]="$id"
    done < "$ids_file"
  fi

  # Three pairing passes, in this order:
  #   identity - the source file's inode and birth time, which a rename keeps
  #              however much the file was edited around it. Only the source
  #              side is ever renamed in place: the destination is regenerated
  #              or re-downloaded, so this pass needs ids_file to remember which
  #              file used to sit at the orphan's path.
  #   basename - survives a relocation, even if the file was edited in transit
  #   mtime    - survives a rename, which changes the basename but not the
  #              timestamp (a Drive move rewrites parents, not modifiedTime,
  #              and touch -r has already copied that onto the counterpart)
  # Only unambiguous 1:1 matches are acted on, in every pass.
  local -A o_n o_s f_n f_s
  local mode key i oi fi from to
  for mode in identity basename mtime; do
    [ "$mode" != identity ] || [ ${#prev_ids[@]} -gt 0 ] || continue
    o_n=(); o_s=(); f_n=(); f_s=()
    for i in "${!orphans[@]}"; do
      stem="${orphans[$i]}"
      [ -n "$stem" ] || continue
      case "$mode" in
        identity) key="${prev_ids[$stem]:-}" ;;
        basename) key="${stem##*/}" ;;
        mtime) key=$(stat -c '%.9Y' "$dst_dir/$stem$dst_ext") ;;
      esac
      [ -n "$key" ] || continue
      o_n[$key]=$(( ${o_n[$key]:-0} + 1 ))
      o_s[$key]="$i"
    done
    for i in "${!fresh[@]}"; do
      stem="${fresh[$i]}"
      [ -n "$stem" ] || continue
      case "$mode" in
        identity) key=$(file_id "$src_dir/$stem$src_ext") ;;
        basename) key="${stem##*/}" ;;
        mtime) key=$(stat -c '%.9Y' "$src_dir/$stem$src_ext") ;;
      esac
      [ -n "$key" ] || continue
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
      if [ -n "$on_move" ] && ! "$on_move" "${orphans[$oi]}$dst_ext" "${fresh[$fi]}$dst_ext"; then
        echo "markdown-sync: not following move of '${orphans[$oi]}$dst_ext', $on_move declined" >&2
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

# file_id <path>
#
# Inode plus birth time: what a rename preserves and a delete + create does
# not. The inode alone would not do, as filesystems recycle inode numbers, and
# a freshly created file inheriting a deleted one's would be "followed" into
# its predecessor's remote copy, sharing settings and all. Prints nothing where
# birth time is unknown, which disables the identity pass for that file.
file_id() {
  local ino birth
  read -r ino birth < <(stat -c '%i %W' "$1")
  [ "${birth:-0}" != 0 ] && [ "$birth" != - ] && printf '%s:%s' "$ino" "$birth"
  return 0
}

# record_ids <dir> <ext> <ids_file>
#
# Snapshot the identity of every <ext> file under <dir>, for the next run's
# mirror_moves to recognise them by after a rename.
record_ids() {
  local dir="$1" ext="$2" out="$3" f id
  mkdir -p "$(dirname "$out")"
  for f in "$dir"/**/*"$ext"; do
    id=$(file_id "$f")
    if [ -n "$id" ]; then
      printf '%s\t%s\0' "$id" "${f#"$dir"/}"
    fi
  done > "$out.tmp"
  mv "$out.tmp" "$out"
}

# follow_remote_move <old_rel> <new_rel>
#
# on_move hook for the pre-sync pass, where the markdown side leads: carry the
# move through to the remote as a server-side move, so the remote keeps the
# *same* file under the new name instead of losing it to a delete + create.
#
# Moving the local file alone is not enough. bisync would still read it as a
# delete + create and replay exactly that on the remote, and for Google Drive
# that means a new file ID: the old document goes to the trash with its
# sharing, comments, revision history and every link to it. --track-renames
# cannot help either: it pairs files by size first, and an imported Google Doc
# reports no size at all.
#
# So move the remote file here, then rename its entry in bisync's listings to
# match. bisync then finds the file unchanged at its new path on both sides, or
# changed only on Path1 if it was edited too, which it uploads in place.
#
# Reads: remote (bisync's Path2 root), rclone (array: the rclone binary plus the
# flags that reach the remote), listing1 and listing2 (bisync's .lst files).
# shellcheck disable=SC2154
follow_remote_move() {
  local old="$1" new="$2" q_old q_new src dst stat lst rc=0

  # No listings yet: there is no record to keep consistent, and the initial
  # resync will pick the file up wherever it ends up.
  [ -f "$listing1" ] && [ -f "$listing2" ] || return 0
  # bisync records paths as Go %q strings. Plain names need only \ and "
  # escaped; anything Go might escape differently is left to the delete +
  # create fallback rather than risk a listing line bisync would misread.
  q_old=$(go_quote "$old") && q_new=$(go_quote "$new") || return 0
  # Never synced to the remote, so there is nothing there to move.
  listing_has "$listing2" "$q_old" || return 0
  if listing_has "$listing1" "$q_new" || listing_has "$listing2" "$q_new"; then
    echo "markdown-sync: '$new' already exists on the remote" >&2
    return 1
  fi

  src=$(remote_path "$old")
  dst=$(remote_path "$new")
  # Look before moving: moveto's exit status cannot tell a source that is gone
  # (it retries it as a directory move, which fails like any other error) from
  # a remote that cannot be reached. A stat can: it prints null for the former.
  stat=$("${rclone[@]}" lsjson --stat --files-only "$src") || rc=$?
  if [ "$rc" -eq 0 ] && [ "$stat" = null ]; then
    # Already gone from the remote, which bisync will notice by itself.
    echo "markdown-sync: '$old' is no longer on the remote, not renaming it there" >&2
    return 0
  fi
  [ "$rc" -ne 0 ] || "${rclone[@]}" moveto "$src" "$dst" || rc=$?
  if [ "$rc" -ne 0 ]; then
    # Most likely offline. Nothing has changed yet on either side, so stop the
    # run here and retry the whole thing next time: carrying on would turn the
    # move into the very delete + create this exists to prevent.
    echo "markdown-sync: failed to rename '$old' on the remote (rclone exit $rc), aborting this run" >&2
    exit 1
  fi

  for lst in "$listing1" "$listing2" "$listing1-old" "$listing2-old"; do
    [ -f "$lst" ] && listing_rename "$lst" "$q_old" "$q_new"
  done
  echo "markdown-sync: renamed on remote: $old -> $new" >&2
}

go_quote() {
  local s="$1"
  case "$s" in *[[:cntrl:]]*) return 1 ;; esac
  s="${s//\\/\\\\}"
  printf '"%s"' "${s//\"/\\\"}"
}

# shellcheck disable=SC2154
remote_path() {
  case "$remote" in
    *: | */) printf '%s%s' "$remote" "$1" ;;
    *) printf '%s/%s' "$remote" "$1" ;;
  esac
}

# The path is a listing line's last field, and starts at its only unescaped ".
listing_has() {
  local line
  while IFS= read -r line; do
    [[ "$line" == *" $2" ]] && return 0
  done < "$1"
  return 1
}

listing_rename() {
  local lst="$1" from="$2" to="$3" line
  while IFS= read -r line; do
    [[ "$line" == *" $from" ]] && line="${line%"$from"}$to"
    printf '%s\n' "$line"
  done < "$lst" > "$lst.tmp"
  mv "$lst.tmp" "$lst"
}
