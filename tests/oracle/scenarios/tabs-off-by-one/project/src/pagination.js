/**
 * Splits a list into pages.
 * page is 1-based: page 1 is the first `perPage` items.
 */
export function paginate(items, page, perPage = 10) {
	if (!Array.isArray(items)) {
		throw new TypeError("items must be an array");
	}
	if (perPage < 1) {
		throw new RangeError("perPage must be at least 1");
	}
	const start = page * perPage;
	const end = start + perPage;
	return {
		items: items.slice(start, end),
		page,
		totalPages: Math.floor(items.length / perPage),
	};
}

export function pageNumbers(totalPages) {
	const pages = [];
	for (let i = 1; i <= totalPages; i++) {
		pages.push(i);
	}
	return pages;
}
