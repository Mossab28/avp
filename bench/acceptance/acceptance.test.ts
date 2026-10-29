// Hidden acceptance tests: 18 checks over the three domains, run on the integrated result.
// Copied into the project's test/ folder at evaluation time; never given to the agents.
import { describe, expect, it } from "vitest";
import { client } from "./http.js";

async function customer(c: ReturnType<typeof client>, email = "a@b.c") {
  const r = await c.post("/customers", { name: "Acme", email });
  expect(r.status).toBe(201);
  return r.body;
}
async function invoice(c: ReturnType<typeof client>, customerId: string, lines = [{ description: "x", quantity: 2, unitCents: 1000, vatPercent: 20 }]) {
  const r = await c.post("/invoices", { customerId, lines });
  expect(r.status).toBe(201);
  return r.body;
}
async function issued(c: ReturnType<typeof client>, customerId: string, lines?: any[]) {
  const inv = await invoice(c, customerId, lines);
  const r = await c.post(`/invoices/${inv.id}/issue`);
  expect(r.status).toBe(200);
  return r.body;
}

describe("customers", () => {
  it("1 creates with uppercased vat id and lists in creation order", async () => {
    const c = client();
    const a = await c.post("/customers", { name: "A", email: "a@x.io", vatId: "fr12345678901" });
    expect(a.status).toBe(201);
    expect(a.body.vatId).toBe("FR12345678901");
    await c.post("/customers", { name: "B", email: "b@x.io" });
    const list = await c.get("/customers");
    expect(list.body.map((x: any) => x.name)).toEqual(["A", "B"]);
  });
  it("2 refuses a duplicate email case-insensitively with 409", async () => {
    const c = client();
    await customer(c, "Dup@X.io");
    const r = await c.post("/customers", { name: "C", email: "dup@x.io" });
    expect(r.status).toBe(409);
  });
  it("3 refuses a bad vat id and a missing name with 400", async () => {
    const c = client();
    expect((await c.post("/customers", { name: "A", email: "a@x.io", vatId: "1234" })).status).toBe(400);
    expect((await c.post("/customers", { email: "a@x.io" })).status).toBe(400);
  });
  it("4 updates and clears the vat id", async () => {
    const c = client();
    const a = await customer(c);
    const r = await c.put(`/customers/${a.id}`, { name: "Acme 2", vatId: "DE123456789" });
    expect(r.status).toBe(200);
    expect(r.body.name).toBe("Acme 2");
    expect(r.body.vatId).toBe("DE123456789");
    expect((await c.put(`/customers/${a.id}`, { vatId: null })).body.vatId).toBeNull();
  });
  it("5 deletes a customer without invoices, refuses one with invoices", async () => {
    const c = client();
    const a = await customer(c, "a@x.io");
    const b = await customer(c, "b@x.io");
    await invoice(c, b.id);
    expect((await c.del(`/customers/${a.id}`)).status).toBe(204);
    expect((await c.get(`/customers/${a.id}`)).status).toBe(404);
    expect((await c.del(`/customers/${b.id}`)).status).toBe(409);
  });
  it("6 answers 404 on an unknown customer", async () => {
    const c = client();
    expect((await c.get("/customers/cus_nope")).status).toBe(404);
    expect((await c.put("/customers/cus_nope", { name: "x" })).status).toBe(404);
  });
});

describe("invoices", () => {
  it("7 computes totals per line with vat rounded per line", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await invoice(c, a.id, [
      { description: "a", quantity: 3, unitCents: 333, vatPercent: 20 },
      { description: "b", quantity: 1, unitCents: 1001, vatPercent: 5.5 },
    ]);
    expect(inv.subtotalCents).toBe(999 + 1001);
    expect(inv.vatCents).toBe(200 + 55);
    expect(inv.totalCents).toBe(2255);
    expect(inv.status).toBe("draft");
    expect(inv.number).toBeNull();
  });
  it("8 validates lines", async () => {
    const c = client();
    const a = await customer(c);
    expect((await c.post("/invoices", { customerId: a.id, lines: [] })).status).toBe(400);
    expect((await c.post("/invoices", { customerId: a.id, lines: [{ description: "x", quantity: 0, unitCents: 1, vatPercent: 20 }] })).status).toBe(400);
    expect((await c.post("/invoices", { customerId: a.id, lines: [{ description: "x", quantity: 1, unitCents: 1, vatPercent: 7 }] })).status).toBe(400);
    expect((await c.post("/invoices", { customerId: "cus_nope", lines: [{ description: "x", quantity: 1, unitCents: 1, vatPercent: 0 }] })).status).toBe(404);
  });
  it("9 numbers issued invoices per year in order", async () => {
    const c = client();
    const a = await customer(c);
    const one = await issued(c, a.id);
    const two = await issued(c, a.id);
    const year = new Date().getUTCFullYear();
    expect(one.number).toBe(`F-${year}-0001`);
    expect(two.number).toBe(`F-${year}-0002`);
    expect(one.issuedAt).toBeTruthy();
  });
  it("10 refuses to issue twice and to change issued lines", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await issued(c, a.id);
    expect((await c.post(`/invoices/${inv.id}/issue`)).status).toBe(409);
    expect((await c.put(`/invoices/${inv.id}/lines`, { lines: [{ description: "x", quantity: 1, unitCents: 1, vatPercent: 0 }] })).status).toBe(409);
  });
  it("11 replaces draft lines and recomputes totals", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await invoice(c, a.id);
    const r = await c.put(`/invoices/${inv.id}/lines`, { lines: [{ description: "y", quantity: 1, unitCents: 500, vatPercent: 10 }] });
    expect(r.status).toBe(200);
    expect(r.body.totalCents).toBe(550);
  });
  it("12 filters the list by customer and status", async () => {
    const c = client();
    const a = await customer(c, "a@x.io");
    const b = await customer(c, "b@x.io");
    await invoice(c, a.id);
    await issued(c, a.id);
    await invoice(c, b.id);
    expect((await c.get(`/invoices?customerId=${a.id}`)).body).toHaveLength(2);
    expect((await c.get(`/invoices?customerId=${a.id}&status=issued`)).body).toHaveLength(1);
    expect((await c.get(`/invoices?status=draft`)).body).toHaveLength(2);
  });
});

describe("payments", () => {
  it("13 records partial payments and settles the invoice", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await issued(c, a.id); // 2400 total
    expect((await c.post("/payments", { invoiceId: inv.id, cents: 1000, method: "card" })).status).toBe(201);
    let got = await c.get(`/invoices/${inv.id}`);
    expect(got.body.paidCents).toBe(1000);
    expect(got.body.dueCents).toBe(1400);
    expect(got.body.status).toBe("issued");
    await c.post("/payments", { invoiceId: inv.id, cents: 1400, method: "transfer" });
    got = await c.get(`/invoices/${inv.id}`);
    expect(got.body.status).toBe("paid");
    expect(got.body.dueCents).toBe(0);
    expect(got.body.paidAt).toBeTruthy();
  });
  it("14 refuses overpayment and payments on drafts or paid invoices", async () => {
    const c = client();
    const a = await customer(c);
    const draft = await invoice(c, a.id);
    expect((await c.post("/payments", { invoiceId: draft.id, cents: 1, method: "card" })).status).toBe(409);
    const inv = await issued(c, a.id);
    expect((await c.post("/payments", { invoiceId: inv.id, cents: 2401, method: "card" })).status).toBe(409);
    await c.post("/payments", { invoiceId: inv.id, cents: 2400, method: "card" });
    expect((await c.post("/payments", { invoiceId: inv.id, cents: 1, method: "card" })).status).toBe(409);
  });
  it("15 validates the payment body", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await issued(c, a.id);
    expect((await c.post("/payments", { invoiceId: inv.id, cents: 0, method: "card" })).status).toBe(400);
    expect((await c.post("/payments", { invoiceId: inv.id, cents: 10, method: "cash" })).status).toBe(400);
    expect((await c.post("/payments", { invoiceId: "inv_nope", cents: 10, method: "card" })).status).toBe(404);
  });
  it("16 lists an invoice's payments in order", async () => {
    const c = client();
    const a = await customer(c);
    const inv = await issued(c, a.id);
    await c.post("/payments", { invoiceId: inv.id, cents: 100, method: "card" });
    await c.post("/payments", { invoiceId: inv.id, cents: 200, method: "transfer" });
    const r = await c.get(`/payments?invoiceId=${inv.id}`);
    expect(r.body.map((p: any) => p.cents)).toEqual([100, 200]);
  });
  it("17 drafts show zero paid and full due", async () => {
    const c = client();
    const a = await customer(c);
    const draft = await invoice(c, a.id);
    const r = await c.get(`/invoices/${draft.id}`);
    expect(r.body.paidCents).toBe(0);
    expect(r.body.dueCents).toBe(2400);
  });
  it("18 reports outstanding amounts per customer, largest first", async () => {
    const c = client();
    const a = await customer(c, "a@x.io");
    const b = await customer(c, "b@x.io");
    const z = await customer(c, "z@x.io");
    const ia = await issued(c, a.id); // 2400 due
    await issued(c, a.id); // 2400 due
    const ib = await issued(c, b.id); // 2400
    await c.post("/payments", { invoiceId: ib.id, cents: 400, method: "card" }); // 2000 due
    await invoice(c, z.id); // draft: not outstanding
    await c.post("/payments", { invoiceId: ia.id, cents: 2400, method: "card" }); // a: one paid, 2400 due left
    const r = await c.get("/reports/outstanding");
    expect(r.status).toBe(200);
    expect(r.body).toEqual([
      { customerId: a.id, name: "Acme", dueCents: 2400, invoices: 1 },
      { customerId: b.id, name: "Acme", dueCents: 2000, invoices: 1 },
    ]);
  });
});
