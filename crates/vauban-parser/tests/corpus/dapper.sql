-- Batches in the style of what a micro-ORM such as Dapper sends: short hand-written
-- statements with named parameters, expanded IN lists (@Ids1, @Ids2, …), INSERT followed
-- by SELECT CAST(SCOPE_IDENTITY() AS int), QueryMultiple batches of several SELECTs.
-- Written from general knowledge of the shapes this tool produces; no
-- table, column or schema of an identifiable project.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch dapper_get_by_id
SELECT Id, Name, Email, CreatedAt FROM Users WHERE Id = @Id

-- @batch dapper_select_star_expanded_in_list
SELECT * FROM Orders WHERE Id IN (@Ids1, @Ids2, @Ids3)

-- @batch dapper_insert_then_scope_identity
INSERT INTO Users (Name, Email, CreatedAt) VALUES (@Name, @Email, @CreatedAt);
SELECT CAST(SCOPE_IDENTITY() AS int)

-- @batch dapper_update_set_where
UPDATE Users SET Name = @Name, Email = @Email, UpdatedAt = GETUTCDATE() WHERE Id = @Id

-- @batch dapper_delete_expired_sessions
DELETE FROM Sessions WHERE UserId = @UserId AND ExpiresAt < @Now

-- @batch dapper_multi_mapping_join
SELECT o.Id, o.OrderDate, o.Total, c.Id, c.Name, c.Email
FROM Orders o
INNER JOIN Customers c ON c.Id = o.CustomerId
WHERE o.Id = @OrderId

-- @batch dapper_query_multiple
SELECT Id, Name FROM Customers WHERE Id = @Id;
SELECT Id, OrderDate, Total FROM Orders WHERE CustomerId = @Id ORDER BY OrderDate DESC;
SELECT COUNT(*) FROM Invoices WHERE CustomerId = @Id AND Paid = 0

-- @batch dapper_paged_with_count
SELECT COUNT(1) FROM Products WHERE CategoryId = @CategoryId;
SELECT Id, Name, Price
FROM Products
WHERE CategoryId = @CategoryId
ORDER BY Name
OFFSET @Offset ROWS FETCH NEXT @PageSize ROWS ONLY

-- @batch dapper_exists_as_case
SELECT CASE WHEN EXISTS (SELECT 1 FROM Users WHERE Email = @Email) THEN 1 ELSE 0 END

-- @batch dapper_insert_multiple_rows
INSERT INTO AuditLog (Entity, EntityId, Action, At)
VALUES (@Entity, @EntityId, N'Created', SYSUTCDATETIME()),
       (@Entity, @EntityId, N'Indexed', SYSUTCDATETIME())

-- @batch dapper_upsert_with_if_exists
IF EXISTS (SELECT 1 FROM Settings WHERE [Key] = @Key)
    UPDATE Settings SET Value = @Value WHERE [Key] = @Key
ELSE
    INSERT INTO Settings ([Key], Value) VALUES (@Key, @Value)

-- @batch dapper_exec_proc_named_args
EXEC dbo.usp_GetOrdersByCustomer @CustomerId = @CustomerId, @Since = @Since

-- @batch dapper_top_nolock_where_1_eq_1
SELECT TOP (50) o.Id, o.Total
FROM dbo.Orders o WITH (NOLOCK)
WHERE 1 = 1 AND o.Status = @Status AND o.Total BETWEEN @Min AND @Max
ORDER BY o.Id DESC

-- @batch dapper_select_where_in_nested_and_like
SELECT u.Id, u.Name
FROM Users u
WHERE u.Id IN (SELECT UserId FROM Memberships WHERE RoleId = @RoleId)
  AND u.Name LIKE @Pattern + '%'
  AND u.Id NOT IN (@Excluded1, @Excluded2)

-- @batch dapper_update_with_case_and_arithmetic
UPDATE Products
SET Stock = Stock - @Quantity,
    Status = CASE WHEN Stock - @Quantity <= 0 THEN 'OutOfStock' ELSE Status END,
    Version = Version + 1
WHERE Id = @Id AND Version = @Version

-- @batch dapper_scalar_with_isnull_and_top
SELECT TOP 1 ISNULL(MAX(Sequence), 0) + 1 FROM Invoices WHERE Year = @Year
