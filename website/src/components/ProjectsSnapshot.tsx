import React, { useState } from "react";
import Link from "@docusaurus/Link";
import snapshot from "@site/src/data/project-snapshot.json";

const date = snapshot.updated_at.slice(0, 10);
const formatCount = (value: number) => value.toLocaleString("en-US");

export function ProjectsCounter() {
  const count = snapshot.discovery_complete ? snapshot.repository_count : snapshot.reported_count;
  return (
    <>
      <Link to="/docs/release-plz-in-the-wild">
        {snapshot.discovery_complete
          ? `${formatCount(count)} observed GitHub Action dependents`
          : `GitHub reports about ${formatCount(count)} dependent repositories`}
      </Link>
      <div style={{ fontSize: "0.85rem" }}>Snapshot: {date}</div>
    </>
  );
}

export default function ProjectsSnapshot() {
  const [sort, setSort] = useState("stars");
  const projects = [...snapshot.projects].sort((a, b) =>
    sort === "stars"
      ? b.stars - a.stars || a.name.localeCompare(b.name, "en")
      : a.name.localeCompare(b.name, "en"),
  );
  return (
    <section aria-label="Projects using release-plz">
      <p>
        GitHub star counts and usage observations from {date}. This snapshot combines public Action
        dependents with maintained examples, including projects using the CLI.
        {snapshot.discovery_complete
          ? ` ${formatCount(snapshot.repository_count)} distinct Action dependents were collected across both Action names.`
          : " Automatic discovery is partial in this cached snapshot; the list is not an exhaustive popularity ranking."}
      </p>
      <p>
        <label>
          Sort projects{" "}
          <select value={sort} onChange={(event) => setSort(event.target.value)}>
            <option value="stars">Most stars</option>
            <option value="name">Project name</option>
          </select>
        </label>
      </p>
      <table>
        <thead>
          <tr>
            <th>Project</th>
            <th>Stars</th>
            <th>Usage evidence</th>
          </tr>
        </thead>
        <tbody>
          {projects.slice(0, 40).map((project) => (
            <tr key={project.name.toLowerCase()}>
              <td>
                <a href={project.url}>{project.name}</a>
              </td>
              <td>{formatCount(project.stars)}</td>
              <td>
                <a href={project.usage_url}>Source</a>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p>Showing up to 40 projects. Counts are cached and may include GitHub-rounded values.</p>
    </section>
  );
}
