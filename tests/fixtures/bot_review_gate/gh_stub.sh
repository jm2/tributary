#!/usr/bin/env bash
# Test double for `gh` used by the bot-review-gate fixture tests. It serves
# recorded API responses from GH_STUB_PAGES (copied fixture data) and counts
# REST calls in GH_STUB_STATE; it never touches a network. An invocation the
# gate script should never make exits 64 so the tests fail loudly.
#
# Behaviour:
#   * `gh api graphql --paginate -f query=...` — the query is fingerprinted
#     by connection name; pages are served in order from
#     GH_STUB_PAGES/{threads,reviews,timeline}/page.N.json, one compact JSON
#     document per page (matching `gh api graphql --paginate` output),
#     stopping after a page whose pageInfo.hasNextPage is false.
#   * `gh api repos/.../pulls/N [--jq <filter>]` — the Nth REST call serves
#     GH_STUB_PAGES/pr.N.json, falling back to pr.1.json.
#   * `gh api repos/.../commits/<sha>/pulls` — the publisher's discovery
#     call; served from GH_STUB_PAGES/commits-head-pulls.json, or the
#     built-in default (open pull request 42 against main, headed by the
#     test HEAD_SHA) when the scenario does not stage one.
#   * `gh api repos/.../check-runs` — the publication call; each invocation
#     is appended to GH_STUB_STATE/check-runs.log as one line of its
#     name/head_sha/status/conclusion fields so tests can assert what the
#     publisher published.
#   * A GH_STUB_PAGES/fail file injects failures: tokens "threads",
#     "reviews", "graphql" fail the matching GraphQL query; "pr" fails REST;
#     "discover" fails the discovery call; "checkrun" fails publication.
set -u

pages="${GH_STUB_PAGES:?GH_STUB_PAGES must be set}"
state="${GH_STUB_STATE:?GH_STUB_STATE must be set}"
fail_mode=""
if [ -f "${pages}/fail" ]; then
  fail_mode="$(cat "${pages}/fail")"
fi

paginate=false
jq_filter=""
query=""
rest_path=""
checkrun_fields=""
prev=""
for arg in "$@"; do
  if [ "${prev}" = "--jq" ]; then
    jq_filter="${arg}"
  elif [ "${prev}" = "-f" ] || [ "${prev}" = "-F" ]; then
    case "${arg}" in
      query=*) query="${arg#query=}" ;;
      name=*|head_sha=*|status=*|conclusion=*)
        checkrun_fields="${checkrun_fields} ${arg}" ;;
    esac
  fi
  case "${arg}" in
    --paginate) paginate=true ;;
    --jq | -f | -F)
      prev="${arg}"
      continue
      ;;
    api) ;;
    repos/*)
      if [ -z "${rest_path}" ]; then rest_path="${arg}"; fi
      ;;
  esac
  prev="${arg}"
done

if [ -n "${rest_path}" ]; then
  case "${rest_path}" in
    *commits/*/pulls*)
      # The publisher's discovery call: the pull requests associated with
      # the announcing run's exact commit.
      case "${fail_mode}" in
        *discover*)
          echo "stub: injected discovery query failure" >&2
          exit 1
          ;;
      esac
      file="${pages}/commits-head-pulls.json"
      if [ -f "${file}" ]; then
        cat "${file}"
      else
        # Default: pull request 42, open against main, headed by the
        # fixture HEAD_SHA — the pull request every scenario evaluates.
        printf '%s\n' '[{"number": 42, "state": "open", "base": {"ref": "main"}, "head": {"sha": "1111111111111111111111111111111111111111"}}]'
      fi
      exit 0
      ;;
    *check-runs*)
      # The publication call: record what was published so tests can
      # assert the verdict and its head binding.
      case "${fail_mode}" in
        *checkrun*)
          echo "stub: injected check-run publication failure" >&2
          exit 1
          ;;
      esac
      printf 'CHECK-RUN%s\n' "${checkrun_fields}" >> "${state}/check-runs.log"
      printf '%s\n' '{"id": 1, "html_url": "stub://check-runs/1"}'
      exit 0
      ;;
    *contents/*)
      # The substitution policy file read: served from contents.json; when
      # that fixture file is absent the stub answers 404 like the real API
      # for a repository without a policy file.
      case "${fail_mode}" in
        *contents*)
          echo "stub: injected contents query failure" >&2
          exit 1
          ;;
      esac
      file="${pages}/contents.json"
      if [ ! -f "${file}" ]; then
        echo "gh: Not Found (HTTP 404)" >&2
        exit 1
      fi
      if [ -n "${jq_filter}" ]; then
        exec jq -r "${jq_filter}" "${file}"
      fi
      cat "${file}"
      exit 0
      ;;
  esac
  case "${fail_mode}" in
    *pr*)
      echo "stub: injected pull request query failure" >&2
      exit 1
      ;;
  esac
  count_file="${state}/rest.count"
  n="$(cat "${count_file}" 2>/dev/null || printf '0')"
  n=$((n + 1))
  printf '%s\n' "${n}" > "${count_file}"
  file="${pages}/pr.${n}.json"
  if [ ! -f "${file}" ]; then file="${pages}/pr.1.json"; fi
  if [ -n "${jq_filter}" ]; then
    exec jq -r "${jq_filter}" "${file}"
  fi
  cat "${file}"
  exit 0
fi

if [ "${paginate}" = true ] && [ -n "${query}" ]; then
  kind=""
  case "${query}" in
    *reviewThreads*first:*) kind="threads" ;;
    *timelineItems*first:*) kind="timeline" ;;
    *reviews*first:*) kind="reviews" ;;
  esac
  if [ -z "${kind}" ]; then
    echo "stub: unrecognised graphql query" >&2
    exit 64
  fi
  case "${fail_mode}" in
    *graphql* | *"${kind}"*)
      echo "stub: injected ${kind} query failure" >&2
      exit 1
      ;;
  esac
  n=1
  while :; do
    file="${pages}/${kind}/page.${n}.json"
    if [ ! -f "${file}" ]; then
      echo "stub: ${kind} page ${n} missing after a page promised another" >&2
      exit 64
    fi
    jq -c . "${file}"
    has_next="$(jq -r '[.. | objects | select(has("hasNextPage")) | .hasNextPage] | any' "${file}")"
    if [ "${has_next}" != "true" ]; then
      exit 0
    fi
    n=$((n + 1))
  done
fi

echo "stub: unexpected invocation: $*" >&2
exit 64
