/**
 * One deployment attempt: what it claimed, what it proved, and what it left
 * behind.
 *
 * An attempt is a receipt, so the page is read as one — a label/value list
 * in the order a reader checks it (which board, whether it worked, what was
 * built, where it went, who ran it), then the artifact identities the finish
 * measured, then the receipt text verbatim. Nothing here is summarised: an
 * attempt whose evidence is paraphrased is an attempt nobody can audit
 * (ADR-039, ADR-043).
 *
 * A field with nothing in it still appears, as an em dash. The absence of a
 * served commit is itself evidence, and a list that dropped its empty rows
 * would make a reader wonder whether the field exists at all.
 */

import { Fragment, type ReactElement, type ReactNode } from "react";
import { stamp } from "../format";
import type { Route } from "../router";
import { useProjection } from "../shell";
import type { DeploymentAttempt } from "./deployments";

/** The identity mode an attempt with no build commit of its own proves. */
const IDENTITY_MODE_ARTIFACT = "artifact";

/** What an empty field says: nothing, said once. */
const ABSENT = "—";

interface DeploymentDetail {
  board: string;
  deployment: DeploymentAttempt;
}

/** The fields, in the order the receipt is read. */
function fields(board: string, row: DeploymentAttempt): Array<[string, ReactNode]> {
  return [
    ["Board", board],
    ["Status", row.status],
    ["Repository", row.repo],
    ["Identity mode", row.identityMode],
    [
      "Build commit",
      row.identityMode === IDENTITY_MODE_ARTIFACT ? (
        <span key="build" className="meta">
          {row.buildCommitLabel}
        </span>
      ) : (
        <code key="build">{row.buildCommit}</code>
      ),
    ],
    [
      "Deployer checkout",
      row.deployerCheckout === null || row.deployerCheckout === undefined ? (
        ABSENT
      ) : (
        <code key="checkout">{row.deployerCheckout}</code>
      ),
    ],
    ["Branch", row.branch ?? ABSENT],
    ["Tier", row.tier],
    ["Environment", row.environment],
    ["Host", row.host],
    ["URL", row.url],
    [
      "Task",
      row.taskID === null || row.taskID === undefined ? (
        ABSENT
      ) : (
        <a
          key="task"
          href={`/task/${encodeURIComponent(board)}/${encodeURIComponent(row.taskID)}`}
          data-ref
          target="_blank"
          rel="noopener"
        >
          {row.taskID}
        </a>
      ),
    ],
    ["Actor", row.actor],
    ["Lane", row.lane ?? ABSENT],
    ["Mechanism", row.mechanism ?? ABSENT],
    ["Retry of", row.retryOf ?? ABSENT],
    ["Phase", row.phase ?? ABSENT],
    [
      "Served commit",
      row.servedCommit !== null && row.servedCommit !== undefined ? (
        <code key="served">{row.servedCommit}</code>
      ) : row.identityMode === IDENTITY_MODE_ARTIFACT ? (
        "not applicable - proved by artifact identity"
      ) : (
        ABSENT
      ),
    ],
    ["Started", stamp(row.createdAt)],
    [
      "Completed",
      row.completedAt === null || row.completedAt === undefined
        ? ABSENT
        : stamp(row.completedAt),
    ],
    ["Archived", row.archived ? "yes" : "no"],
  ];
}

export default function DeploymentPage({ route }: { route: Route }): ReactElement {
  const board = route.params.project ?? "";
  const id = route.params.id ?? "";
  const { data, error } = useProjection<DeploymentDetail>(
    `/api/v1/deployment/${encodeURIComponent(board)}/${encodeURIComponent(id)}`,
  );

  return (
    <main id="main" data-page data-route="deployment" data-testid="app-root">
      {error !== null || data === null ? (
        <>
          <h1>Deployment</h1>
          {error === null ? null : (
            // The route's own refusal is non-enumerating: an attempt that is
            // absent and one this reader may not see answer the same way.
            <p className="error" data-testid="deployment-refused">
              No deployment at that address.
            </p>
          )}
        </>
      ) : (
        <>
          <h1
            data-deployment-detail={data.deployment.id}
            data-testid="deployment-detail"
          >
            Deployment <code>{data.deployment.id}</code>
          </h1>
          <dl>
            {fields(data.board, data.deployment).map(([label, value]) => (
              <Fragment key={label}>
                <dt>{label}</dt>
                <dd data-deployment-field={label}>{value}</dd>
              </Fragment>
            ))}
          </dl>
          {data.deployment.artifacts.length === 0 ? null : (
            <>
              <h2>Artifact identities</h2>
              <table data-testid="deployment-artifacts">
                <thead>
                  <tr>
                    <th>Role</th>
                    <th>Kind</th>
                    <th>Expected</th>
                    <th>Observed</th>
                  </tr>
                </thead>
                <tbody>
                  {data.deployment.artifacts.map((artifact) => (
                    <tr key={`${artifact.role}/${artifact.kind}/${artifact.expected}`}>
                      <td>{artifact.role}</td>
                      <td>{artifact.kind}</td>
                      <td>
                        <code>{artifact.expected}</code>
                      </td>
                      <td>
                        {artifact.observed === null ||
                        artifact.observed === undefined ? (
                          "not yet observed"
                        ) : (
                          <code>{artifact.observed}</code>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </>
          )}
          <h2>Receipt</h2>
          <pre data-deployment-receipt data-testid="deployment-receipt">
            {data.deployment.receipt ?? "No terminal receipt yet."}
          </pre>
          {data.deployment.artifactUri === null ||
          data.deployment.artifactUri === undefined ? null : (
            <p className="meta">
              Artifact: <code>{data.deployment.artifactUri}</code>
            </p>
          )}
        </>
      )}
    </main>
  );
}
