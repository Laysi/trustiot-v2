import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import type { ScheduleInput } from "../../api/pending/reports.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { apiError } from "../../test/fake-identity.ts";
import { FakeReportServer } from "../../test/fake-reports.ts";
import { signedInBrowser } from "../../test/render.tsx";
import { renderReports } from "../../test/render-reports.tsx";
import { FIRST_USE_DONE } from "./onboarding.ts";
import { editSchedulePath, NEW_SCHEDULE_PATH, WEEKLY_REPORT_ONBOARDING_PATH } from "./paths.ts";

afterEach(() => {
  cleanup();
});

const SELF = "alice@example.com";

function open(
  server: FakeReportServer,
  path: string,
  options: { role?: "viewer" | "editor" | "admin"; devices?: FakeDeviceServer } = {},
) {
  return renderReports(path, server, {
    identity: signedInBrowser({ role: options.role ?? "admin" }).browser,
    ...(options.devices ? { devices: options.devices.api() } : {}),
  });
}

function form(name: string): HTMLElement {
  return screen.getByRole("form", { name });
}

function recipientBox(): HTMLInputElement {
  return screen.getByRole("textbox", { name: "Recipients" }) as HTMLInputElement;
}

function typeRecipient(text: string, key: "Enter" | "," = "Enter") {
  act(() => {
    fireEvent.change(recipientBox(), { target: { value: text } });
  });
  act(() => {
    fireEvent.keyDown(recipientBox(), { key });
  });
}

function submit(name: string) {
  act(() => {
    fireEvent.submit(form(name));
  });
}

/** The input of the last `createSchedule` call. */
function created(server: FakeReportServer): ScheduleInput | undefined {
  return server.calls.filter((call) => call.endpoint === "createSchedule").at(-1)?.args[0] as ScheduleInput | undefined;
}

describe("a new schedule", () => {
  test("starts as the weekly report to the user, every device, Monday 08:00, and previews the first send", async () => {
    open(new FakeReportServer(), NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    expect((screen.getByRole("radio", { name: /Weekly energy report/ }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByRole("combobox", { name: "Send on" }) as HTMLSelectElement).value).toBe("1");
    expect((screen.getByLabelText("Time") as HTMLInputElement).value).toBe("08:00");
    expect(screen.getByText(`${SELF} (you)`)).toBeTruthy();
    expect(screen.getByText("UTC (your preferred time zone)")).toBeTruthy();
    const preview = screen.getByText(/^The first report goes out on /);
    expect(preview.textContent).toMatch(/on \d\d-\d\d \(Mon\) at 08:00 and covers \d\d-\d\d – \d\d-\d\d\.$/);
  });

  test("saves what was filled in, with the time zone of the preferences, and goes back to the list with a note", async () => {
    const server = new FakeReportServer();
    const { router } = open(server, NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    typeRecipient("chen.wei@example.com");
    typeRecipient("plant-mgr@example.com", ",");
    act(() => {
      fireEvent.change(screen.getByRole("combobox", { name: "Send on" }), { target: { value: "3" } });
      fireEvent.change(screen.getByLabelText("Time"), { target: { value: "07:30" } });
    });
    submit("New report schedule");
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports"));
    expect(created(server)).toEqual({
      kind: "weekly_energy",
      scope: { kind: "all" },
      send_on: 3,
      time: "07:30",
      time_zone: "UTC",
      recipients: [SELF, "chen.wei@example.com", "plant-mgr@example.com"],
    });
    expect(await screen.findByText("The schedule was saved.")).toBeTruthy();
    expect(server.schedules).toHaveLength(1);
  });

  test("an address typed and not yet entered is saved too", async () => {
    const server = new FakeReportServer();
    const { router } = open(server, NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    act(() => {
      fireEvent.change(recipientBox(), { target: { value: "boss@example.com" } });
    });
    submit("New report schedule");
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports"));
    expect(created(server)?.recipients).toEqual([SELF, "boss@example.com"]);
  });

  test("the monthly report sends on a day of the month", async () => {
    open(new FakeReportServer(), `${NEW_SCHEDULE_PATH}?kind=monthly_carbon`);
    await screen.findByRole("form", { name: "New report schedule" });
    expect((screen.getByRole("radio", { name: /Monthly carbon report/ }) as HTMLInputElement).checked).toBe(true);
    const day = screen.getByRole("combobox", { name: "Send on" }) as HTMLSelectElement;
    expect(within(day).getByRole("option", { name: "Day 1 of the month" })).toBeTruthy();
    expect(day.options).toHaveLength(28);
    expect(screen.getByText(/^The first report goes out on \d\d-01 /).textContent).toMatch(
      /covers \d\d-01 – \d\d-\d\d\.$/,
    );
  });

  test("an address that is not one stays in the box with the problem, described and marked invalid", async () => {
    const server = new FakeReportServer();
    open(server, NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    typeRecipient("chen@@example");
    const box = recipientBox();
    expect(box.value).toBe("chen@@example");
    expect(box.getAttribute("aria-invalid")).toBe("true");
    const message = screen.getByText(`"chen@@example" isn't a valid email address.`);
    expect(box.getAttribute("aria-describedby")).toContain(message.id);
    submit("New report schedule");
    const summary = await screen.findByRole("alert");
    expect(summary.textContent).toContain("1 things need fixing");
    expect(within(summary).getByRole("button", { name: /^Recipients: / })).toBeTruthy();
    expect(created(server)).toBeUndefined();
    // Pressing the line moves the focus to the box.
    act(() => {
      fireEvent.click(within(summary).getByRole("button", { name: /^Recipients: / }));
    });
    expect(document.activeElement).toBe(box);
  });

  test("a schedule needs a recipient, and a device when devices are chosen", async () => {
    const server = new FakeReportServer();
    const devices = new FakeDeviceServer();
    devices.addDevice({ name: "line-2-meter" });
    devices.addDevice({ name: "compressor-1" });
    const { router } = open(server, NEW_SCHEDULE_PATH, { devices });
    await screen.findByRole("form", { name: "New report schedule" });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: `Remove ${SELF}` }));
      fireEvent.change(screen.getByRole("combobox", { name: "Devices" }), { target: { value: "devices" } });
    });
    submit("New report schedule");
    const summary = await screen.findByRole("alert");
    expect(summary.textContent).toContain("2 things need fixing");
    expect(screen.getByText("Add at least one recipient.")).toBeTruthy();
    expect(screen.getByText("Choose at least one device.")).toBeTruthy();
    expect(created(server)).toBeUndefined();

    // Fixing a field takes its problem away; the rest stay until they are fixed.
    typeRecipient("lin.mei@example.com");
    expect(screen.queryByText("Add at least one recipient.")).toBeNull();
    act(() => {
      fireEvent.click(screen.getByRole("checkbox", { name: "compressor-1" }));
    });
    expect(screen.queryByText("Choose at least one device.")).toBeNull();
    submit("New report schedule");
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports"));
    const [compressor] = [...devices.devices.values()].filter((device) => device.name === "compressor-1");
    expect(created(server)?.scope).toEqual({ kind: "devices", device_ids: [compressor?.device_id ?? ""] });
  });

  test("a recipient the server rejects is shown at the recipients, from its field error", async () => {
    const server = new FakeReportServer();
    server.failNext(
      "createSchedule",
      apiError(422, "validation_failed", "error.validation_failed", [
        { field: "/recipients/1", message_key: "reports.recipient_invalid", details: { email: "x@y.zz" } },
      ]),
    );
    const { router } = open(server, NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    submit("New report schedule");
    expect(await screen.findByText(`"x@y.zz" isn't a valid email address.`)).toBeTruthy();
    expect(router.state.location.pathname).toBe(NEW_SCHEDULE_PATH);
    expect(recipientBox().getAttribute("aria-invalid")).toBe("true");
  });

  test("a refusal that is not about a field is shown above the form and keeps what was typed", async () => {
    const server = new FakeReportServer();
    server.failNext("createSchedule", apiError(500, "internal", "error.internal"));
    open(server, NEW_SCHEDULE_PATH);
    await screen.findByRole("form", { name: "New report schedule" });
    typeRecipient("boss@example.com");
    submit("New report schedule");
    expect((await screen.findByRole("alert")).textContent).toContain("Something went wrong on our side");
    expect(screen.getByText("boss@example.com")).toBeTruthy();
  });

  test("a plan without reports gets the wall instead of a form", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    open(server, NEW_SCHEDULE_PATH);
    expect(await screen.findByRole("heading", { level: 2, name: /Business plan and above/ })).toBeTruthy();
    expect(screen.queryByRole("form")).toBeNull();
  });

  test("a viewer who opens the address is told why there is no form", async () => {
    open(new FakeReportServer(), NEW_SCHEDULE_PATH, { role: "viewer" });
    expect(await screen.findByRole("heading", { name: "Only editors can set up schedules" })).toBeTruthy();
    expect(screen.queryByRole("form")).toBeNull();
    expect(screen.getByRole("link", { name: "Back to the reports" }).getAttribute("href")).toBe("/reports");
  });
});

describe("from the setup checklist", () => {
  test("is the weekly report, filled in, with a note on the step", async () => {
    open(new FakeReportServer(), WEEKLY_REPORT_ONBOARDING_PATH);
    expect(await screen.findByRole("heading", { level: 1, name: "Set up the weekly report" })).toBeTruthy();
    expect(screen.getByText(/Setup checklist, step 4/)).toBeTruthy();
    const box = form("Set up the weekly report");
    expect((within(box).getByRole("radio", { name: /Weekly energy report/ }) as HTMLInputElement).checked).toBe(true);
    expect(within(box).getByText(`${SELF} (you)`)).toBeTruthy();
    expect(within(box).getByRole("link", { name: "Back to the conversation" }).getAttribute("href")).toBe("/");
  });

  test("saving it goes back to the conversation home with the step done", async () => {
    const server = new FakeReportServer();
    const { router } = open(server, WEEKLY_REPORT_ONBOARDING_PATH);
    await screen.findByRole("form", { name: "Set up the weekly report" });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Save and finish this step" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(router.state.location.state).toEqual(FIRST_USE_DONE);
    expect(created(server)).toMatchObject({ kind: "weekly_energy", send_on: 1, time: "08:00", recipients: [SELF] });
  });

  test("a plan without reports does not get the checklist's form", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    open(server, WEEKLY_REPORT_ONBOARDING_PATH);
    expect(await screen.findByRole("heading", { level: 2, name: /Business plan and above/ })).toBeTruthy();
  });
});

describe("changing a schedule", () => {
  function seeded() {
    const server = new FakeReportServer();
    const schedule = server.addSchedule(
      {
        kind: "monthly_carbon",
        send_on: 5,
        time: "09:15",
        recipients: ["lin.mei@example.com", "plant-mgr@example.com"],
      },
      { bounced_recipients: ["plant-mgr@example.com"] },
    );
    return { server, schedule };
  }

  test("opens with the saved values; the report's type cannot change", async () => {
    const { server, schedule } = seeded();
    open(server, editSchedulePath(schedule.id));
    await screen.findByRole("form", { name: "Edit report schedule" });
    expect((screen.getByRole("radio", { name: /Monthly carbon report/ }) as HTMLInputElement).checked).toBe(true);
    // The fieldset around the radios is disabled, which disables each of them in a browser.
    const group = screen.getByRole("radio", { name: /Weekly energy report/ }).closest("fieldset");
    expect(group?.disabled).toBe(true);
    expect((screen.getByRole("combobox", { name: "Send on" }) as HTMLSelectElement).value).toBe("5");
    expect((screen.getByLabelText("Time") as HTMLInputElement).value).toBe("09:15");
    expect(screen.getByText("plant-mgr@example.com")).toBeTruthy();
  });

  test("saves the change to that schedule, which clears the bounce, and says so on the list", async () => {
    const { server, schedule } = seeded();
    const { router } = open(server, editSchedulePath(schedule.id));
    await screen.findByRole("form", { name: "Edit report schedule" });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Remove plant-mgr@example.com" }));
      fireEvent.change(screen.getByLabelText("Time"), { target: { value: "10:00" } });
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports"));
    const call = server.calls.find((entry) => entry.endpoint === "updateSchedule");
    expect(call?.args[0]).toBe(schedule.id);
    expect(call?.args[1]).toMatchObject({
      kind: "monthly_carbon",
      send_on: 5,
      time: "10:00",
      recipients: ["lin.mei@example.com"],
    });
    expect(await screen.findByText("The schedule was changed.")).toBeTruthy();
    expect(screen.queryByText(/bounced/)).toBeNull();
  });

  test("an unknown schedule is a not-found page", async () => {
    open(new FakeReportServer(), editSchedulePath("00000000-0000-4000-8000-00000000dead"));
    expect(await screen.findByRole("heading", { name: "Report not found" })).toBeTruthy();
  });

  test("a failed load can be retried", async () => {
    const { server, schedule } = seeded();
    server.failNext("getSchedule", apiError(500, "internal", "error.internal"));
    open(server, editSchedulePath(schedule.id));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load the schedule");
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByRole("form", { name: "Edit report schedule" })).toBeTruthy();
  });
});
