/**
 * Deployments: what is actually running, what is landing, and what broke.
 *
 * Three groups because they answer three different questions and are read at
 * three different moments — the current release is a fact to trust, an
 * attempt in flight is a thing to wait for, and a failure is work. They
 * carry three separate caps for the same reason, and each says whether it
 * was cut rather than leaving a reader to wonder (ADR-037 §4): a matrix that
 * silently stopped at a hundred rows is a matrix an operator plans around
 * wrongly.
 *
 * Current releases are a table because they are compared column by column
 * across repositories; the other two are rows of prose, because what matters
 * about an attempt is the sentence, not the grid.
 */

import type { ReactElement } from "react";
import { ago } from "../format";
import { useProjection } from "../shell";

/** The identity mode an attempt with no build commit of its own proves. */
const IDENTITY_MODE_ARTIFACT = "artifact";

/** `rust/model.rs`'s `DeploymentAttempt`, as the projection serialises it. */
export interface DeploymentAttempt {
  id: string;
  taskID?: string | null;
  repo: string;
  identityMode: string;
  buildCommit: string;
  buildCommitLabel: string;
  deployerCheckout?: string | null;
  branch?: string | null;
  tier: string;
  environment: string;
  host: string;
  url: string;
  mechanism?: string | null;
  retryOf?: string | null;
  status: string;
  phase?: string | null;
  actor: string;
  lane?: string | null;
  receipt?: string | null;
  artifactUri?: string | null;
  servedCommit?: string | null;
  artifacts: ArtifactVerification[];
  createdAt: number;
  updatedAt: number;
  completedAt?: number | null;
  archived: boolean;
}

/** One expected artifact identity, and what the finish observed for it. */
export interface ArtifactVerification {
  role: string;
  kind: string;
  expected: string;
  observed?: string | null;
}

interface BoardDeployment {
  board: string;
  deployment: DeploymentAttempt;
}

interface Envelope {
  items: BoardDeployment[];
  limit: number | null;
  truncated: boolean;
}

interface DeploymentIndex {
  current: Envelope;
  active: Envelope;
  failures: Envelope;
}

/**
 * What one attempt's build-commit cell says (ADR-043 §4).
 *
 * A Git attempt shows the first twelve characters of its commit. An artifact
 * attempt has no commit to show and says so in words rather than printing a
 * sentinel that reads like a SHA.
 */
export function BuildCommit({
  deployment,
}: {
  deployment: DeploymentAttempt;
}): ReactElement {
  if (deployment.identityMode === IDENTITY_MODE_ARTIFACT) {
    return <span className="meta">{deployment.buildCommitLabel}</span>;
  }
  return <code>{deployment.buildCommit.slice(0, 12)}</code>;
}

/**
 * The attempt's own address. It keeps `data-ref` and opens in a tab of its
 * own, exactly as the served page's did: a reference is previewed in place
 * and followed without losing the page it was read from (SPA-40).
 */
export function AttemptLink({
  board,
  deployment,
}: {
  board: string;
  deployment: DeploymentAttempt;
}): ReactElement {
  return (
    <a
      href={`/deployment/${encodeURIComponent(board)}/${encodeURIComponent(deployment.id)}`}
      data-deployment-link={deployment.id}
      data-ref
      target="_blank"
      rel="noopener"
    >
      <code>{deployment.id}</code>
    </a>
  );
}

/** Said once per group, and only when the group was actually cut. */
function Truncation({
  envelope,
  per,
  testId,
}: {
  envelope: Envelope;
  per: string;
  testId: string;
}): ReactElement | null {
  if (!envelope.truncated) {
    return null;
  }
  return (
    <p className="meta" data-testid={testId}>
      Cut at {envelope.limit} {per}. <code>kb deploy list --all</code> reaches the rest.
    </p>
  );
}

export default function DeploymentsPage(): ReactElement {
  const { data, error } = useProjection<DeploymentIndex>("/api/v1/deployments");
  const current = data?.current.items ?? [];
  const active = data?.active.items ?? [];
  const failures = data?.failures.items ?? [];

  return (
    <main id="main" data-page data-route="deployments" data-testid="app-root">
      <div className="heading">
        <h1>Deployments</h1>
      </div>
      <p className="meta">
        Verified current releases, derived from immutable attempts. Old non-current
        terminal attempts self-archive from hot views, and{" "}
        <code>kb deploy list --all</code> still reaches them.
      </p>
      {error === null ? null : (
        <p className="error" data-testid="deployments-error">
          The deployment ledger could not be read. Reload the page to try again.
        </p>
      )}
      <h2>Current releases</h2>
      {data === null ? null : current.length === 0 ? (
        <p className="empty" data-testid="deployments-none-current">
          No verified release has been recorded yet.
        </p>
      ) : (
        <table data-testid="deployments-current">
          <thead>
            <tr>
              <th>Repository</th>
              <th>Tier</th>
              <th>Environment</th>
              <th>Commit</th>
              <th>Host</th>
              <th>Attempt</th>
              <th>Verified</th>
            </tr>
          </thead>
          <tbody>
            {current.map(({ board, deployment }) => (
              <tr
                key={`${board}/${deployment.id}`}
                data-current-release={deployment.id}
                data-testid="deployment-current-row"
              >
                <td>
                  {deployment.repo}
                  <div className="meta">{board}</div>
                </td>
                <td>
                  <code>{deployment.tier}</code>
                </td>
                <td>{deployment.environment}</td>
                <td>
                  <BuildCommit deployment={deployment} />
                </td>
                <td>{deployment.host}</td>
                <td>
                  <AttemptLink board={board} deployment={deployment} />
                </td>
                <td className="when">
                  {ago(deployment.completedAt ?? deployment.updatedAt)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <Truncation
        envelope={data?.current ?? { items: [], limit: null, truncated: false }}
        per="current releases per board"
        testId="deployments-current-cut"
      />
      <h2>In progress</h2>
      {data === null ? null : active.length === 0 ? (
        <p className="empty" data-testid="deployments-none-active">
          No deployment is currently in progress.
        </p>
      ) : (
        active.map(({ board, deployment }) => (
          <article
            className="item"
            key={`${board}/${deployment.id}`}
            data-deployment-active={deployment.id}
            data-testid="deployment-active"
          >
            <p>
              <AttemptLink board={board} deployment={deployment} />{" "}
              <strong>{deployment.repo}</strong> to the <code>{deployment.tier}</code>{" "}
              {deployment.environment}
            </p>
            <p className="meta">
              <BuildCommit deployment={deployment} /> on {deployment.host}, started{" "}
              {ago(deployment.createdAt)} by {deployment.actor}
            </p>
          </article>
        ))
      )}
      <Truncation
        envelope={data?.active ?? { items: [], limit: null, truncated: false }}
        per="attempts in flight per board"
        testId="deployments-active-cut"
      />
      <h2>Recent failures</h2>
      {data === null ? null : failures.length === 0 ? (
        <p className="empty" data-testid="deployments-none-failures">
          No failed or abandoned attempt is in the hot window.
        </p>
      ) : (
        failures.map(({ board, deployment }) => (
          <article
            className="item"
            key={`${board}/${deployment.id}`}
            data-deployment-failure={deployment.id}
            data-testid="deployment-failure"
          >
            <p>
              <AttemptLink board={board} deployment={deployment} />{" "}
              <strong>{deployment.repo}</strong>{" "}
              <span
                className={`pill status-${deployment.status}`}
                data-testid="deployment-failure-status"
              >
                {deployment.status}
              </span>
            </p>
            <p className="meta">
              The {deployment.tier} tier, {deployment.environment}, in phase{" "}
              {deployment.phase ?? "unknown"} {ago(deployment.updatedAt)}
            </p>
            <p className="body" data-testid="deployment-failure-receipt">
              {deployment.receipt ?? "No receipt recorded."}
            </p>
          </article>
        ))
      )}
      <Truncation
        envelope={data?.failures ?? { items: [], limit: null, truncated: false }}
        per="failed or abandoned attempts per board per status"
        testId="deployments-failures-cut"
      />
    </main>
  );
}
