#!/usr/bin/env bash
set -eu
shopt -s nullglob
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
LOCKFILES="$SCRIPT_DIR/../Cargo.lock $SCRIPT_DIR/../gui/package-lock.json"
# The policy of enforcing lockfiles to be signed was not in place before this commit and as such some of the commits before are not signed
WHITELIST_COMMIT="bdf327cfa"

for LOCKFILE in $LOCKFILES;
do
    lockfile_commit_hashes=$(git log --oneline $WHITELIST_COMMIT..HEAD --follow $LOCKFILE | awk '{print $1}')
    unsigned_commits_exist="false"
    for commit in $lockfile_commit_hashes;
    do
        if ! $(git verify-commit $commit 2> /dev/null); then
            echo Commit $commit is not signed
            unsigned_commits_exist="true"
        fi
    done
done

if [[ $unsigned_commits_exist == "true" ]]; then
    echo "Unsigned commits exist"
    exit 1
fi
