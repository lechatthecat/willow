package main
import ("fmt"; "os")
func main() {
 if os.Args[1] == "word_count" {
  words := make([]string,4096); for i:=range words { words[i]=fmt.Sprintf("token_%d_%d",i,i*31) }
  counts:=make(map[string]int64)
  fmt.Println("@start work")
  for i:=0;i<6000000;i++ { w:=words[i%4096]; v:=counts[w]; counts[w]=v+1 }
  fmt.Println("@end work")
  var sum int64; for _,w:=range words {sum+=counts[w]}; fmt.Println(sum)
 } else {
  values:=make(map[int64]int64); var sum int64
  fmt.Println("@start work")
  for i:=int64(0);i<3000000;i++ {values[i]=i}
  for i:=int64(0);i<12000000;i++ {sum+=values[i%3000000]}
  fmt.Println("@end work"); fmt.Println(sum)
 }
}
