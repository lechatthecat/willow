package main
import("fmt";"os";"runtime")
type Node struct { value int64; next *Node }; type Point struct {x,y int64}
type Shape interface{ area() int64 };type Square struct{s int64};func(s *Square)area()int64{return s.s*s.s};type Rect struct{w,h int64};func(r *Rect)area()int64{return r.w*r.h}
func churn(n,size int64)int64 {r:=make([]*Point,size);for i:=range r{r[i]=&Point{}};var sum int64;for i:=int64(0);i<n;i++{sum+=r[i%size].x;r[i%size]=&Point{i,i}};runtime.KeepAlive(r);return sum}
func main(){runtime.GOMAXPROCS(8);var sum int64;fmt.Println("@start");switch os.Args[1]{
case "linked_list":var head *Node;for i:=int64(0);i<3000000;i++{head=&Node{i,head}};fmt.Println("@setup");for r:=0;r<5;r++{for p:=head;p!=nil;p=p.next{sum+=p.value}};runtime.KeepAlive(head)
case "object_churn":sum=churn(20000000,1024)
case "gc_scheduler":done:=make(chan int64);for i:=0;i<8;i++{go func(){done<-churn(1500000,2048)}()};for i:=0;i<8;i++{sum+=<-done}
case "virtual_dispatch":a:=make([]Shape,0);for i:=int64(0);i<1000000;i++{n:=i%10+1;if i%2==0{a=append(a,&Square{n})}else{a=append(a,&Rect{n,n+1})}};fmt.Println("@setup");for _,s:=range a{sum+=s.area()};runtime.KeepAlive(a)
};fmt.Println("@end");fmt.Println(sum)}
