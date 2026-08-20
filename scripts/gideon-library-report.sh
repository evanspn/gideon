#!/bin/sh
# Report what is actually in a gideon library: every profile, its books, its
# reading progress, and whether settings.json agrees with the disk.
#
# READ-ONLY. This script opens files and counts things. It never creates,
# moves, renames or deletes anything — run it on a device you are worried
# about without adding to the worry.
#
# Usage:
#   scripts/gideon-library-report.sh [library-dir] [settings-dir]
#
# Defaults are the Kobo install paths:
#   library  /mnt/onboard/Manga
#   settings /mnt/onboard/.adds/gideon/data
#
# Over USB from a computer, point it at the mounted volume instead:
#   scripts/gideon-library-report.sh /Volumes/KOBOeReader/Manga \
#                                    /Volumes/KOBOeReader/.adds/gideon/data
#
# What it answers:
#   * which profiles exist on disk (each is a "@name" directory)
#   * how many .cbz files and how much data each one holds
#   * how many chapters each one has reading progress for
#   * whether a profile exists on disk but is missing from settings.json —
#     the case where the picker would not show it even though every book is
#     still there
set -eu

library=${1:-/mnt/onboard/Manga}
settings_dir=${2:-/mnt/onboard/.adds/gideon/data}
settings="$settings_dir/settings.json"

if [ ! -d "$library" ]; then
	echo "No library directory at $library" >&2
	echo "Pass the right path as the first argument." >&2
	exit 1
fi

# Books and bytes under a directory, not counting other profiles' "@" dirs.
# Prints "<count> <kilobytes>".
profile_totals() {
	dir=$1
	# Prune "@" directories BELOW dir only ("$dir/@*"), never dir itself — a
	# profile's own directory is named "@name" and pruning it would report
	# every profile as empty.
	count=$(find "$dir" -path "$dir/@*" -prune -o -type f -name '*.cbz' -print 2>/dev/null | wc -l)
	# -exec, not xargs: manga paths are full of spaces and xargs would split
	# them into filenames that don't exist, silently under-counting.
	kb=$(find "$dir" -path "$dir/@*" -prune -o -type f -exec du -sk {} + 2>/dev/null |
		awk '{total += $1} END {print total + 0}')
	echo "$count ${kb:-0}"
}

# Human-readable size from kilobytes, so a small library doesn't read as "0 MB".
human_size() {
	kb=$1
	if [ "$kb" -ge 1048576 ]; then
		echo "$((kb / 1048576)).$(((kb % 1048576) * 10 / 1048576)) GB"
	elif [ "$kb" -ge 1024 ]; then
		echo "$((kb / 1024)).$(((kb % 1024) * 10 / 1024)) MB"
	else
		echo "$kb KB"
	fi
}

# Number of chapters recorded in a progress.json, counting "current_page" keys.
# Deliberately crude: no jq on the device, and a count is all we need here.
progress_entries() {
	file=$1
	[ -f "$file" ] || { echo 0; return; }
	tr ',' '\n' <"$file" | grep -c 'current_page' || echo 0
}

report_profile() {
	name=$1
	dir=$2
	totals=$(profile_totals "$dir")
	books=$(echo "$totals" | cut -d' ' -f1)
	kb=$(echo "$totals" | cut -d' ' -f2)
	progress=$(progress_entries "$dir/.gideon/progress.json")

	echo "  profile: $name"
	echo "    directory:   $dir"
	echo "    books:       $books cbz files, $(human_size "$kb")"
	echo "    progress:    $progress chapters with reading progress"
	if [ -f "$dir/.gideon/sync_session.json" ]; then
		echo "    sync:        signed in"
	fi
}

echo "gideon library report"
echo "  library:  $library"
echo "  settings: $settings"
echo

# The library root itself is the "default" profile's library when it still
# holds books directly (before a conversion moves them into an @ directory).
root_books=$(profile_totals "$library" | cut -d' ' -f1)
on_disk=""
if [ "$root_books" -gt 0 ] || [ -d "$library/.gideon" ]; then
	echo "Profiles on disk:"
	report_profile "default (the library root)" "$library"
	on_disk="default"
else
	echo "Profiles on disk:"
fi

for dir in "$library"/@*; do
	[ -d "$dir" ] || continue
	name=$(basename "$dir" | cut -c2-)
	report_profile "$name" "$dir"
	on_disk="$on_disk $name"
done

if [ -z "$on_disk" ]; then
	echo "  (none — this library has no books and no profile directories)"
fi

echo
if [ -f "$settings" ]; then
	listed=$(tr -d ' \t\n' <"$settings" |
		sed -n 's/.*"profiles":\[\([^]]*\)\].*/\1/p' |
		tr ',' '\n' | tr -d '"')
	if [ -z "$listed" ]; then
		echo "settings.json lists no profiles (missing, truncated, or unparseable)."
	else
		echo "settings.json lists: $(echo "$listed" | tr '\n' ' ')"
	fi
	for name in $on_disk; do
		[ "$name" = "default" ] && continue
		if ! echo "$listed" | grep -qx "$name"; then
			echo
			echo "  NOTE: \"$name\" has a library on disk but settings.json does not"
			echo "  list it, so the profile picker may not show it. Its books are"
			echo "  fine — every one of them is in $library/@$name."
			echo "  Current gideon builds rediscover such a profile by themselves;"
			echo "  on an older build, add \"$name\" to the \"profiles\" list in"
			echo "  settings.json and it comes back."
		fi
	done
else
	echo "No settings.json at $settings — the app will start with defaults."
	echo "Every profile directory listed above still holds all of its books."
fi
