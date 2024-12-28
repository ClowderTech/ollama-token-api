import { Hono } from "hono";
import { bearerAuth } from "hono/bearer-auth";
import { logger } from "hono/logger";
import { MongoClient } from "mongodb";

const app = new Hono();

interface TokenDoc {
	token: string;
	input: number;
	output: number;
}

// Connect to MongoDB
const client = new MongoClient(Deno.env.get("MONGODB_URI")!);
client.connect(); // Make sure to connect before using

const db = client.db("ollama");

function removeTrailingSlash(str: string): string {
	// Check if the string ends with a slash and remove it if present
	return str.endsWith("/") ? str.slice(0, -1) : str;
}

app.use(logger());

app.use(
	"*",
	bearerAuth({
		verifyToken: async (token, _c) => {
			const tokenDoc = await db.collection<TokenDoc>("tokens").findOne({
				token,
			});

			if (!tokenDoc) {
				return false;
			}

			return true;
		},
	}),
	async (c) => {
		const ollama_url = removeTrailingSlash(Deno.env.get("OLLAMA_URL")!);

		try {
			const response = await fetch(`${ollama_url}${c.req.path}`, {
				method: c.req.method,
				headers: c.req.header(),
				body: c.req.method === "GET" || c.req.method === "HEAD"
					? undefined
					: await c.req.arrayBuffer(),
			});

			// const jsonResponce = await response.json()

			// if (jsonResponce["prompt_eval_count"] && jsonResponce["eval_count"]) {
			// 	const token = c.req.header("Authorization")!.replace("Bearer ", "")

			// 	const tokenDoc = await db.collection<TokenDoc>("tokens").findOne({
			// 		token
			// 	});

			// }

			return response;
		} catch (error) {
			console.error("Error forwarding request:", error);
			return c.json({ message: "Internal Server Error" }, 500);
		}
	},
);

Deno.serve(app.fetch);
