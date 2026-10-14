import { expect, test } from "bun:test";
import { main } from "../src/cli.js";
import { tempDir } from "./helpers.js";

test("the commands, then the report", (done) => {
  const dir = tempDir();
  main(dir, ["add-user", "ana"], (err, line) => {
    expect(line).toBe("added ana");
    main(dir, ["visit"], () => {
      main(dir, ["set", "title", "Weekly"], () => {
        main(dir, ["report"], (err2, report) => {
          expect(report).toBe("Weekly: 1 users (ana); 1 visits");
          done();
        });
      });
    });
  });
});
