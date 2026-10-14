paginate() in src/pagination.js is broken: page 1 should return the first perPage items, and totalPages should count a partly filled last page (25 items at 10 per page is 3 pages). Fix it.
