import java.util.*;
public class AllocationBench {
 static class Node {long value;Node next;Node(long v,Node n){value=v;next=n;}}
 record Point(long x,long y){} interface Shape{long area();} record Square(long s)implements Shape{public long area(){return s*s;}}record Rect(long w,long h)implements Shape{public long area(){return w*h;}}
 static long churn(int n,int size){Point[] r=new Point[size];Arrays.setAll(r,i->new Point(0,0));long sum=0;for(int i=0;i<n;i++){sum+=r[i%size].x();r[i%size]=new Point(i,i);}return sum;}
 public static void main(String[] args)throws Exception{long sum=0;System.out.println("@start");switch(args[0]){
 case "linked_list":{Node head=null;for(int i=0;i<3000000;i++)head=new Node(i,head);System.out.println("@setup");for(int r=0;r<5;r++)for(Node p=head;p!=null;p=p.next)sum+=p.value;break;}
 case "object_churn":sum=churn(20000000,1024);break;
 case "gc_scheduler":{long[] sums=new long[8];Thread[] ts=new Thread[8];for(int i=0;i<8;i++){final int k=i;ts[i]=Thread.startVirtualThread(()->sums[k]=churn(1500000,2048));}for(Thread t:ts)t.join();for(long x:sums)sum+=x;break;}
 case "virtual_dispatch":{ArrayList<Shape>a=new ArrayList<>();for(int i=0;i<1000000;i++){long n=i%10+1;a.add(i%2==0?new Square(n):new Rect(n,n+1));}System.out.println("@setup");for(Shape s:a)sum+=s.area();break;}
 }System.out.println("@end");System.out.println(sum);}
}
