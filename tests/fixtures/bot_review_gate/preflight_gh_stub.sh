#!/usr/bin/env bash
# Test double for `gh` used by the preflight validator's live-path tests. It
# serves recorded API documents from GH_STUB_PAGES and never touches a network.
#
# Documents are keyed by the API path with the owner/repo prefix stripped and
# slashes replaced by underscores, e.g.
#   repos/jm2/tributary/rules/branches/main        -> rules_branches_main.json
#   repos/jm2/tributary/rulesets/17650907          -> rulesets_17650907.json
#   repos/jm2/tributary/environments/ENV/secrets   -> environments_ENV_secrets.json
#
# Paginated list endpoints (branch rules, secrets listings, deployment branch
# policies) serve `<slug>.page.<N>.json`. A missing page beyond the first
# answers with an empty page so the validator's pagination loop terminates; a
# missing first page answers 404 like the real API. A GH_STUB_PAGES/fail file
# injects non-404 read failures: a line `error:<slug>` makes every page of that
# endpoint fail with an API-style error.
set -u

pages="${GH_STUB_PAGES:?GH_STUB_PAGES must be set}"
state="${GH_STUB_STATE:?GH_STUB_STATE must be set}"
printf '%s\n' "$*" >> "${state}/invocations.log"

path=""
for arg in "$@"; do
  case "${arg}" in
    repos/*)
      path="${arg}"
      break
      ;;
  esac
done
if [ -z "${path}" ]; then
  echo "stub: no repos/ path in: $*" >&2
  exit 64
fi

query=""
case "${path}" in
  *\?*)
    query="${path#*\?}"
    path="${path%%\?*}"
    ;;
esac
page=1
case "${query}" in
  *page=*)
    # `##` strips through the LAST `page=` so the `page` inside `per_page` is
    # not mistaken for the pagination parameter.
    page="${query##*page=}"
    page="${page%%&*}"
    ;;
esac

slug="${path#repos/}"
slug="${slug#*/}"
slug="${slug#*/}"
slug="$(printf '%s' "${slug}" | tr '/' '_')"

if [ -f "${pages}/fail" ] && grep -Fqx "error:${slug}" "${pages}/fail"; then
  echo "gh: HTTP 500: injected read failure for ${slug}" >&2
  exit 1
fi

page_file="${pages}/${slug}.page.${page}.json"
base_file="${pages}/${slug}.json"

paged="no"
case "${slug}" in
  rules_branches_main | *_secrets | *_deployment-branch-policies) paged="yes" ;;
esac

if [ -f "${page_file}" ]; then
  cat "${page_file}"
  exit 0
fi
# A paginated list past its last recorded page answers with an empty page so
# the validator's pagination loop terminates. The page-1/base file is not a
# fallback for a later page: replaying it would fabricate a duplicate item and
# mask a truncated inventory.
if [ "${paged}" = "yes" ] && [ "${page}" -gt 1 ]; then
  case "${slug}" in
    rules_branches_main) printf '[]\n' ;;
    *_secrets) printf '{"total_count": 0, "secrets": []}\n' ;;
    *_deployment-branch-policies) printf '{"total_count": 0, "branch_policies": []}\n' ;;
  esac
  exit 0
fi
if [ -f "${base_file}" ]; then
  cat "${base_file}"
  exit 0
fi
echo "gh: Not Found (HTTP 404)" >&2
exit 1
