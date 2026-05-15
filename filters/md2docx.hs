{-# LANGUAGE OverloadedStrings #-}
import Text.Pandoc.JSON

main :: IO ()
main = toJSONFilter md2docx

-- Strip auto-generated heading IDs so docx doesn't accumulate stale bookmarks
md2docx :: Block -> Block
md2docx (Header level (_, classes, kvs) inlines) =
  Header level ("", classes, kvs) inlines
-- Convert tight lists to loose so Google Docs renders proper bullet formatting
md2docx (BulletList items) = BulletList $ map expandItem items
md2docx (OrderedList attrbs items) = OrderedList attrbs $ map expandItem items
md2docx blk = blk

-- Promote Plain to Para so each list item becomes its own styled paragraph in docx
expandItem :: [Block] -> [Block]
expandItem [Plain bs] = [Para bs]
expandItem item       = item
