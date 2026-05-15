import Text.Pandoc.JSON

main :: IO ()
main = toJSONFilter docx2md

-- Convert loose lists to tight for cleaner rendering in Obsidian
docx2md :: Block -> Block
docx2md (BulletList items) = BulletList $ map compactItem items
docx2md (OrderedList attrbs items) = OrderedList attrbs $ map compactItem items
docx2md blk = blk

-- Demote Para to Plain so pandoc writes list items without blank lines between them
compactItem :: [Block] -> [Block]
compactItem [Para bs] = [Plain bs]
compactItem item      = item
